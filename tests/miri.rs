//! The tests of uscope's unsafe code, which `just miri` runs under Miri
//! as well as natively. They are a test target of their own so that Miri
//! checks only this file, not the whole crate, each time it runs them.

#![allow(unsafe_code, reason = "these tests drive unsafe code directly")]

mod allocator {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicPtr, Ordering};

    use uscope::timings::alloc::{
        Allocated, Counting, PEAK_GRANULARITY, start_totals, stop_totals, thread_counts, totals,
    };

    /// Where the tests' blocks come from: the system's allocator; under
    /// Miri, Rust's, which Miri serves itself and so checks that every
    /// block is freed with the layout it was allocated with, which the
    /// system's `free` would not.
    #[cfg(not(miri))]
    type Blocks = std::alloc::System;
    #[cfg(miri)]
    type Blocks = Rust;

    /// Rust's global allocator.
    #[cfg(miri)]
    struct Rust;

    // SAFETY: every method forwards to the global allocator with the
    // caller's arguments.
    #[cfg(miri)]
    unsafe impl GlobalAlloc for Rust {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc`.
            unsafe { std::alloc::alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc_zeroed`.
            unsafe { std::alloc::alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller passes a block the global allocator
            // returned, with its layout.
            unsafe { std::alloc::dealloc(block, layout) }
        }

        unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller passes a block the global allocator
            // returned, its layout, and a valid new size.
            unsafe { std::alloc::realloc(block, layout, new_size) }
        }
    }

    /// Allocates from [`Blocks`], but refuses to grow any block.
    struct NoGrowth;

    // SAFETY: allocation forwards to `Blocks`; refusing a reallocation by
    // returning null is allowed and leaves the block valid.
    unsafe impl GlobalAlloc for NoGrowth {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc`.
            unsafe { Blocks {}.alloc(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller passes a block `Blocks` returned.
            unsafe { Blocks {}.dealloc(block, layout) }
        }

        unsafe fn realloc(&self, _: *mut u8, _: Layout, _: usize) -> *mut u8 {
            std::ptr::null_mut()
        }
    }

    /// This thread's counts, from its slot. Other threads of a test
    /// process, such as the harness's, may allocate while a test measures,
    /// and their counts can reach it late, so tests read their own threads'
    /// counts exactly and the process's totals only as bounds.
    fn own() -> (Allocated, i64) {
        thread_counts().expect("a slot of the thread's own")
    }

    #[test]
    fn totals_follow_blocks_across_threads_and_failed_growth() {
        let allocator = Counting::new(NoGrowth);
        // A block that moves the peak by more than a thread's step, so the
        // harness's own allocations cannot hide it.
        let size = 2 * usize::try_from(PEAK_GRANULARITY).expect("a small size");
        let bytes = u64::try_from(size).expect("a small size");
        let layout = Layout::from_size_align(size, 8).expect("a layout");
        start_totals();
        let before = totals();
        // Another thread allocates while this one measures the process's
        // totals, as the test harness's own threads may.
        let other_layout = Layout::from_size_align(48, 8).expect("a layout");
        let other = std::thread::scope(|scope| {
            scope
                // SAFETY: the layout is non-zero; the block is freed below.
                .spawn(|| AtomicPtr::new(unsafe { allocator.alloc(other_layout) }))
                .join()
                .expect("the other thread")
        });
        let (allocated, live) = own();
        // SAFETY: the layout is non-zero; the block is freed below.
        let block = unsafe { allocator.alloc(layout) };
        assert!(!block.is_null());
        let (grown, grown_live) = own();
        assert_eq!(grown.since(&allocated), Allocated { blocks: 1, bytes });
        assert_eq!(grown_live - live, 2 * PEAK_GRANULARITY);
        let total = totals().since(&before).allocated;
        assert!(
            total.blocks >= 2 && total.bytes >= bytes + 48,
            "the process's totals miss a block: {total:?}"
        );

        // SAFETY: the block came from `allocator` with `layout`.
        let refused = unsafe { allocator.realloc(block, layout, 2 * size) };
        assert!(refused.is_null());
        assert_eq!(
            own(),
            (grown, grown_live),
            "a failed growth allocates nothing"
        );

        // Another thread frees the block, and the heap shrinks all the
        // same.
        let block = AtomicPtr::new(block);
        let (start, end) = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let start = own();
                    // SAFETY: the block came from `allocator` with `layout`
                    // and is freed once.
                    unsafe { allocator.dealloc(block.load(Ordering::Relaxed), layout) };
                    (start, own())
                })
                .join()
                .expect("the freeing thread")
        });
        let after = totals();
        stop_totals();
        assert_eq!(end.0, start.0, "freeing allocates nothing");
        assert_eq!(end.1 - start.1, -2 * PEAK_GRANULARITY);
        assert!(
            after.since(&before).peak_bytes >= PEAK_GRANULARITY,
            "{after:?}"
        );
        // SAFETY: the other thread's block came from `allocator` with
        // `other_layout` and is freed once.
        unsafe { allocator.dealloc(other.load(Ordering::Relaxed), other_layout) };
    }

    #[test]
    fn blocks_keep_their_contents_and_counts_as_they_grow_and_shrink() {
        let allocator = Counting::new(Blocks {});
        let pattern: [u8; 16] = *b"0123456789abcdef";
        let layout = |size| Layout::from_size_align(size, 8).expect("a layout");
        let start = own();
        // SAFETY: the layout is non-zero; the block is freed below.
        let block = unsafe { allocator.alloc_zeroed(layout(16)) };
        assert!(!block.is_null());
        let zeroed = own();
        // SAFETY: the block holds 16 initialized bytes that nothing else
        // refers to.
        let contents = unsafe { std::slice::from_raw_parts_mut(block, 16) };
        assert_eq!(contents, [0; 16]);
        contents.copy_from_slice(&pattern);

        // SAFETY: the block came from `allocator` with this layout.
        let block = unsafe { allocator.realloc(block, layout(16), 64) };
        assert!(!block.is_null());
        let grown = own();
        // SAFETY: growth kept the block's first 16 bytes, which nothing
        // else refers to.
        let contents = unsafe { std::slice::from_raw_parts(block, 16) };
        assert_eq!(contents, pattern);

        // SAFETY: the block came from `allocator` with this layout.
        let block = unsafe { allocator.realloc(block, layout(64), 8) };
        assert!(!block.is_null());
        let shrunk = own();
        // SAFETY: shrinking kept the block's first 8 bytes.
        let contents = unsafe { std::slice::from_raw_parts(block, 8) };
        assert_eq!(contents, &pattern[..8]);

        // SAFETY: the block came from `allocator` with this layout.
        unsafe { allocator.dealloc(block, layout(8)) };
        let freed = own();

        let counted =
            |(allocated, live): (Allocated, i64)| (allocated.since(&start.0), live - start.1);
        let allocated = |blocks, bytes| Allocated { blocks, bytes };
        assert_eq!(counted(zeroed), (allocated(1, 16), 16));
        assert_eq!(
            counted(grown),
            (allocated(2, 64), 64),
            "a growth counts as a block of what it added"
        );
        assert_eq!(counted(shrunk), (allocated(2, 64), 8));
        assert_eq!(counted(freed), (allocated(2, 64), 0));
    }
}

mod ring {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU64, Ordering};

    use uscope::unsafe_parts::{DATA_HEAD, DATA_TAIL, HEAP_DATA, RingMemory};

    /// What the kernel does to a ring: writes records past the head, then
    /// publishes the head, and reuses only bytes the tail has passed.
    struct Kernel {
        base: NonNull<u8>,
        size: u64,
        head: u64,
    }

    // SAFETY: the kernel's side writes only data the reader's tail has
    // passed, and the metadata words atomically, as the kernel does.
    unsafe impl Send for Kernel {}

    impl Kernel {
        const fn word(&self, offset: usize) -> &AtomicU64 {
            // SAFETY: the word lies in the ring's metadata, aligned, for
            // the ring's life, and both sides access it atomically.
            unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(offset).cast()) }
        }

        fn tail(&self) -> u64 {
            self.word(DATA_TAIL).load(Ordering::Acquire)
        }

        /// Writes `bytes` past the head, wrapping, unpublished.
        fn write(&mut self, bytes: &[u8]) {
            assert!(self.head + bytes.len() as u64 - self.tail() <= self.size);
            for &byte in bytes {
                let at = HEAP_DATA + usize::try_from(self.head % self.size).expect("small");
                // SAFETY: the byte lies in the ring's data, which the
                // reader does not read until the head is published.
                unsafe { self.base.as_ptr().add(at).write(byte) };
                self.head += 1;
            }
        }

        fn publish(&self) {
            self.word(DATA_HEAD).store(self.head, Ordering::Release);
        }
    }

    fn ring(size: usize) -> (RingMemory, Kernel) {
        let (memory, base) = RingMemory::heap(size);
        let kernel = Kernel {
            base,
            size: size as u64,
            head: 0,
        };
        (memory, kernel)
    }

    fn bytes(from: u8, count: u8) -> Vec<u8> {
        (from..from + count).collect()
    }

    #[test]
    fn an_empty_ring_drains_nothing_and_unpublished_records_wait() {
        let (mut memory, mut kernel) = ring(64);
        let mut out = vec![7];
        assert_eq!(memory.drain(&mut out), 0);
        kernel.write(&bytes(0, 16));
        assert_eq!(memory.drain(&mut out), 0, "the head is not yet published");
        assert_eq!(out, [7]);
        assert_eq!(kernel.tail(), 0);
    }

    #[test]
    fn records_that_wrap_follow_what_was_drained_before() {
        let (mut memory, mut kernel) = ring(64);
        let mut out = Vec::new();
        kernel.write(&bytes(0, 48));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 48);
        assert_eq!(kernel.tail(), 48);
        // 16 bytes to the end, then 24 from the start.
        kernel.write(&bytes(48, 40));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 40);
        assert_eq!(out, bytes(0, 88));
        assert_eq!(kernel.tail(), 88);
    }

    #[test]
    fn a_full_ring_drains_whole() {
        let (mut memory, mut kernel) = ring(64);
        kernel.write(&bytes(0, 8));
        kernel.publish();
        let mut out = Vec::new();
        memory.drain(&mut out);
        kernel.write(&bytes(8, 64));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 64);
        assert_eq!(out, bytes(0, 72));
    }

    /// A head further ahead than the ring holds is not the kernel's: the
    /// ring reads nothing and gives nothing back.
    #[test]
    fn a_head_past_a_whole_ring_drains_nothing() {
        let (mut memory, kernel) = ring(64);
        kernel.word(DATA_HEAD).store(65, Ordering::Release);
        let mut out = Vec::new();
        assert_eq!(memory.drain(&mut out), 0);
        assert!(out.is_empty());
        assert_eq!(kernel.tail(), 0);
    }

    /// The kernel writes as the ring drains: every record arrives once,
    /// whole, in order. Run with many seeds, Miri tries the orderings the
    /// memory model allows.
    #[test]
    fn concurrent_records_arrive_once_whole_and_in_order() {
        const RECORDS: u8 = 24;
        let (mut memory, mut kernel) = ring(32);
        let writer = std::thread::spawn(move || {
            for record in 0..RECORDS {
                while kernel.head + 8 - kernel.tail() > kernel.size {
                    std::thread::yield_now();
                }
                kernel.write(&[record; 8]);
                kernel.publish();
            }
        });
        let mut out = Vec::new();
        while out.len() < usize::from(RECORDS) * 8 {
            if memory.drain(&mut out) == 0 {
                std::thread::yield_now();
            }
        }
        writer.join().expect("the kernel's side");
        let expected: Vec<u8> = (0..RECORDS).flat_map(|record| [record; 8]).collect();
        assert_eq!(out, expected);
    }
}

mod thread_db {
    use std::ffi::{c_int, c_long, c_void};
    use std::ptr;

    use nix::libc;
    use nix::unistd::Pid;
    use uscope::unsafe_parts::{
        Handle, ProcessServices, ps_get_thread_area, ps_getpid, ps_lgetregs, ps_pdread,
        ps_pglobal_lookup, ps_ptread,
    };

    const PS_OK: c_int = 0;
    const PS_ERR: c_int = 1;
    const PS_BADLID: c_int = 3;
    const PS_NOSYM: c_int = 5;
    const FS: c_int = 25;

    /// A process of one thread, 41, with 16 bytes at 0x1000 and one
    /// symbol. Reading 0xdead panics.
    struct Process;

    const MEMORY: [u8; 16] = *b"0123456789abcdef";

    impl ProcessServices for Process {
        fn read(&self, address: u64, output: &mut [u8]) -> bool {
            assert_ne!(address, 0xdead, "a service that panics");
            let Some(start) = address.checked_sub(0x1000) else {
                return false;
            };
            let start = usize::try_from(start).unwrap();
            let Some(bytes) = MEMORY.get(start..start + output.len()) else {
                return false;
            };
            output.copy_from_slice(bytes);
            true
        }

        fn registers(&self, lwp: Pid) -> Option<libc::user_regs_struct> {
            // Each register holds its index in libthread_db's order.
            (lwp.as_raw() == 41).then_some(libc::user_regs_struct {
                r15: 0,
                r14: 1,
                r13: 2,
                r12: 3,
                rbp: 4,
                rbx: 5,
                r11: 6,
                r10: 7,
                r9: 8,
                r8: 9,
                rax: 10,
                rcx: 11,
                rdx: 12,
                rsi: 13,
                rdi: 14,
                orig_rax: 15,
                rip: 16,
                cs: 17,
                eflags: 18,
                rsp: 19,
                ss: 20,
                fs_base: 21,
                gs_base: 22,
                ds: 23,
                es: 24,
                fs: 25,
                gs: 26,
            })
        }

        fn lookup_symbol(&self, object: Option<&str>, symbol: &str) -> Option<u64> {
            (object.is_none_or(|object| object == "libc.so.6") && symbol == "_thread_db_dtv")
                .then_some(0x2000)
        }
    }

    /// What owns a handle while `libthread_db` holds its pointer, as an
    /// agent does.
    struct Agent<'a> {
        _handle: Handle<'a>,
    }

    const fn at(address: usize) -> *const c_void {
        ptr::without_provenance(address)
    }

    /// `libthread_db` keeps the pointer it was given while the handle moves
    /// into its agent, and calls back through it for the agent's life.
    #[test]
    fn callbacks_answer_through_a_handle_that_moved() {
        let handle = Handle::new(7, &Process);
        let process = handle.as_ptr();
        let agent = Box::new(Agent { _handle: handle });

        // SAFETY: each call passes the handle's pointer, buffers of the
        // sizes it names, and NUL-terminated names.
        unsafe {
            assert_eq!(ps_getpid(process), 7);

            let mut bytes = [0_u8; 4];
            let output = bytes.as_mut_ptr().cast();
            assert_eq!(ps_pdread(process, at(0x1004), output, 4), PS_OK);
            assert_eq!(&bytes, b"4567");
            assert_eq!(ps_ptread(process, at(0x100c), output, 4), PS_OK);
            assert_eq!(&bytes, b"cdef");
            assert_eq!(ps_pdread(process, at(0x100e), output, 4), PS_ERR);
            assert_eq!(ps_pdread(process, at(0xdead), output, 4), PS_ERR);
            assert_eq!(
                ps_pdread(process, at(0x1000), ptr::null_mut(), 0),
                PS_OK,
                "an empty read touches nothing"
            );

            let mut address = ptr::null_mut();
            let symbol = c"_thread_db_dtv".as_ptr();
            assert_eq!(
                ps_pglobal_lookup(process, c"libc.so.6".as_ptr(), symbol, &raw mut address),
                PS_OK
            );
            assert_eq!(address.addr(), 0x2000);
            address = ptr::null_mut();
            assert_eq!(
                ps_pglobal_lookup(process, ptr::null(), symbol, &raw mut address),
                PS_OK
            );
            assert_eq!(address.addr(), 0x2000);
            let other = c"libm.so.6".as_ptr();
            assert_eq!(
                ps_pglobal_lookup(process, other, symbol, &raw mut address),
                PS_NOSYM
            );
            assert_eq!(
                ps_pglobal_lookup(process, other, ptr::null(), &raw mut address),
                PS_ERR
            );

            let mut registers = [-1 as c_long; 27];
            assert_eq!(ps_lgetregs(process, 41, registers.as_mut_ptr()), PS_OK);
            assert!(registers.iter().copied().eq(0..27), "{registers:?}");
            assert_eq!(ps_lgetregs(process, 42, registers.as_mut_ptr()), PS_BADLID);

            let mut area = ptr::null_mut();
            assert_eq!(ps_get_thread_area(process, 41, FS, &raw mut area), PS_OK);
            assert_eq!(area.addr(), 21);
            assert_eq!(ps_get_thread_area(process, 41, 0, &raw mut area), PS_ERR);
            assert_eq!(
                ps_get_thread_area(process, 42, FS, &raw mut area),
                PS_BADLID
            );
        }
        drop(agent);
    }
}
