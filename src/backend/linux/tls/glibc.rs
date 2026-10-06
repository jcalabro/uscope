//! Thread-local storage addresses read through glibc's own layout
//! descriptors, for a C library whose version `libthread_db` refuses.
//!
//! glibc exports a `_thread_db_*` descriptor for every structure field its
//! `libthread_db` reads: the field's width in bits, its element count, and
//! its offset. `libthread_db` reads its layout from these descriptors too,
//! but refuses any C library whose version string differs from its own. This
//! module follows glibc's `td_thr_tlsbase` step by step, reading every
//! offset from the inferior's descriptors, so a C library of any version
//! describes itself.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::unistd::Pid;

use super::{ProcessServices, TlsError, failed};

/// The C library the descriptors are looked up in. Lookups fall back to any
/// module that defines them, as statically linked programs require.
const C_LIBRARY: &str = "libc.so.6";
/// A loader with more TLS slotinfo lists than this is corrupt.
const MAX_SLOTINFO_LISTS: usize = 1 << 16;
/// `TLS_DTV_UNALLOCATED` and every other odd DTV pointer mark a block the
/// thread has not allocated.
const UNALLOCATED_BIT: u64 = 1;
/// `l_tls_offset` values that name no static TLS block.
const NO_TLS_OFFSET: u64 = 0;
const FORCED_DYNAMIC_TLS_OFFSET: u64 = u64::MAX;

static FORCED: AtomicBool = AtomicBool::new(false);

/// Makes every glibc TLS lookup in this process use this module rather than
/// `libthread_db`, so tests can check that the two agree.
pub(super) fn force(forced: bool) {
    FORCED.store(forced, Ordering::Relaxed);
}

pub(super) fn forced() -> bool {
    FORCED.load(Ordering::Relaxed)
}

/// One `_thread_db_*` field descriptor.
#[derive(Debug, Clone, Copy)]
struct Field {
    name: &'static str,
    bits: u32,
    /// The element count of an array field; zero for a flexible array.
    count: u32,
    offset: i32,
}

impl Field {
    /// The address of element `index` of this field in the structure at
    /// `base`.
    fn address(self, base: u64, index: u64) -> Result<u64, TlsError> {
        if self.count != 0 && index >= u64::from(self.count) {
            return Err(failed(format!(
                "index {index} is outside {}, which has {} elements",
                self.name, self.count
            )));
        }
        index
            .checked_mul(u64::from(self.bits / 8))
            .and_then(|element| {
                base.checked_add_signed(i64::from(self.offset))?
                    .checked_add(element)
            })
            .ok_or_else(|| failed(format!("{} at {base:#x} overflows", self.name)))
    }
}

struct Glibc<'a> {
    services: &'a dyn ProcessServices,
}

impl Glibc<'_> {
    fn symbol(&self, name: &str) -> Result<u64, TlsError> {
        self.services
            .lookup_symbol(C_LIBRARY, name)
            .ok_or_else(|| failed(format!("the C library does not define {name}")))
    }

    fn read(&self, address: u64, output: &mut [u8]) -> Result<(), TlsError> {
        if self.services.read(address, output) {
            Ok(())
        } else {
            Err(failed(format!(
                "the C library's thread state at {address:#x} is unreadable"
            )))
        }
    }

    fn field(&self, name: &'static str) -> Result<Field, TlsError> {
        let mut words = [0; 12];
        self.read(self.symbol(name)?, &mut words)?;
        let word = |index: usize| {
            u32::from_le_bytes(
                words[index * 4..index * 4 + 4]
                    .try_into()
                    .expect("four bytes"),
            )
        };
        let field = Field {
            name,
            bits: word(0),
            count: word(1),
            offset: i32::from_le_bytes(word(2).to_le_bytes()),
        };
        // A zero width marks a field this C library does not have. Element
        // widths are whole bytes, and x86-64 descriptors are never
        // byte-swapped.
        if field.bits == 0 || !field.bits.is_multiple_of(8) {
            return Err(failed(format!(
                "{name} describes an unusable {}-bit field",
                field.bits
            )));
        }
        Ok(field)
    }

    /// Reads element `index` of `field` in the structure at `base`,
    /// zero-extended.
    fn fetch(&self, field: Field, base: u64, index: u64) -> Result<u64, TlsError> {
        // Only these widths are values; wider fields are structures.
        if !matches!(field.bits, 8 | 32 | 64) {
            return Err(failed(format!(
                "{} is a {}-bit field, not a value",
                field.name, field.bits
            )));
        }
        let address = field.address(base, index)?;
        let mut bytes = [0; 8];
        let width = usize::try_from(field.bits / 8).expect("field width fits usize");
        self.read(address, &mut bytes[..width])?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn fetch_named(&self, name: &'static str, base: u64, index: u64) -> Result<u64, TlsError> {
        self.fetch(self.field(name)?, base, index)
    }

    /// The head of the loader's list of DTV slotinfo arrays.
    fn slotinfo_lists(&self) -> Result<u64, TlsError> {
        // A dynamically linked C library points at the loader's global
        // state; a static one holds the list in its own variable.
        if let Ok(pointer) = self.symbol("__nptl_rtld_global") {
            let rtld_global = self.fetch_named("_thread_db___nptl_rtld_global", pointer, 0)?;
            return self.fetch_named(
                "_thread_db_rtld_global__dl_tls_dtv_slotinfo_list",
                rtld_global,
                0,
            );
        }
        let variable = self.symbol("_dl_tls_dtv_slotinfo_list")?;
        self.fetch_named("_thread_db__dl_tls_dtv_slotinfo_list", variable, 0)
    }

    /// The address of module `module`'s DTV slotinfo entry.
    fn slotinfo(&self, module: u64) -> Result<u64, TlsError> {
        let length = self.field("_thread_db_dtv_slotinfo_list_len")?;
        let next = self.field("_thread_db_dtv_slotinfo_list_next")?;
        let entries = self.field("_thread_db_dtv_slotinfo_list_slotinfo")?;
        let mut list = self.slotinfo_lists()?;
        let mut first = 0_u64;
        let mut visited = BTreeSet::new();
        while list != 0 {
            if visited.len() == MAX_SLOTINFO_LISTS || !visited.insert(list) {
                return Err(failed("the loader's TLS slotinfo lists form a cycle"));
            }
            let entries_here = self.fetch(length, list, 0)?;
            let end = first
                .checked_add(entries_here)
                .ok_or_else(|| failed("the loader's TLS slotinfo list lengths overflow"))?;
            if module < end {
                return entries.address(list, module - first);
            }
            first = end;
            list = self.fetch(next, list, 0)?;
        }
        Err(failed(format!(
            "the loader has no TLS slot for module {module}"
        )))
    }

    /// The thread's static TLS block of the module at `map`, if it has one.
    fn static_block(&self, thread_pointer: u64, map: u64) -> Result<u64, TlsError> {
        let offset = self.fetch_named("_thread_db_link_map_l_tls_offset", map, 0)?;
        if offset == NO_TLS_OFFSET || offset == FORCED_DYNAMIC_TLS_OFFSET {
            return Err(TlsError::Deferred);
        }
        // x86-64 places static blocks below the thread pointer.
        thread_pointer.checked_sub(offset).ok_or_else(|| {
            failed(format!(
                "static TLS offset {offset:#x} exceeds the thread pointer"
            ))
        })
    }

    /// Follows `td_thr_tlsbase`: the module's block in the thread's DTV
    /// when the DTV is current and the block allocated, and otherwise its
    /// static block.
    fn block(&self, thread_pointer: u64, module: u64) -> Result<u64, TlsError> {
        let entry = self.slotinfo(module)?;
        let map = self.fetch_named("_thread_db_dtv_slotinfo_map", entry, 0)?;
        if map == 0 {
            return Err(failed(format!("TLS module {module} is not loaded")));
        }
        let generation = self.fetch_named("_thread_db_dtv_slotinfo_gen", entry, 0)?;
        let dtv = self.fetch_named("_thread_db_pthread_dtvp", thread_pointer, 0)?;
        let slots = self.field("_thread_db_dtv_dtv")?;
        let counter = slots.address(dtv, 0)?;
        let dtv_generation = self.fetch_named("_thread_db_dtv_t_counter", counter, 0)?;
        // A DTV older than the module may not have its slot at all.
        if dtv_generation < generation {
            return self.static_block(thread_pointer, map);
        }
        let slot = slots.address(dtv, module)?;
        let block = self.fetch_named("_thread_db_dtv_t_pointer_val", slot, 0)?;
        if block & UNALLOCATED_BIT != 0 {
            return self.static_block(thread_pointer, map);
        }
        Ok(block)
    }
}

/// The address of `offset` within the TLS block of the module whose loader
/// `link_map` is given, for `thread`.
pub(super) fn tls_address(
    services: &dyn ProcessServices,
    thread: Pid,
    link_map: u64,
    offset: u64,
) -> Result<u64, TlsError> {
    let glibc = Glibc { services };
    let module = glibc.fetch_named("_thread_db_link_map_l_tls_modid", link_map, 0)?;
    if module == 0 {
        return Err(TlsError::NoTls);
    }
    // x86-64 glibc keeps the thread descriptor at the thread pointer.
    let thread_pointer = services
        .registers(thread)
        .ok_or_else(|| failed(format!("the registers of thread {thread} are unavailable")))?
        .fs_base;
    if thread_pointer == 0 {
        return Err(TlsError::Deferred);
    }
    glibc
        .block(thread_pointer, module)?
        .checked_add(offset)
        .ok_or_else(|| failed("the TLS address overflows"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nix::libc;

    use super::*;
    use crate::backend::linux::core_dump::{GENERAL_REGISTER_COUNT, user_registers};

    const THREAD_POINTER: u64 = 0x7000_0000;
    const DTV: u64 = 0x6000_0000;
    const LINK_MAP: u64 = 0x5000_0000;
    const RTLD_GLOBAL: u64 = 0x4000_0000;
    const FIRST_LIST: u64 = 0x3000_0000;
    const SECOND_LIST: u64 = 0x3100_0000;
    const NPTL_RTLD_GLOBAL: u64 = 0x2000_0000;
    const DESCRIPTORS: u64 = 0x1000_0000;
    const FIRST_LIST_LENGTH: u64 = 64;

    /// A process whose C library lays its structures out unlike any real
    /// glibc, so every offset must come from the descriptors.
    struct Process {
        memory: BTreeMap<u64, u8>,
        symbols: BTreeMap<&'static str, u64>,
        thread_pointer: u64,
    }

    impl ProcessServices for Process {
        fn read(&self, address: u64, output: &mut [u8]) -> bool {
            output.iter_mut().zip(address..).all(|(byte, address)| {
                self.memory
                    .get(&address)
                    .map(|value| *byte = *value)
                    .is_some()
            })
        }

        fn registers(&self, _: Pid) -> Option<libc::user_regs_struct> {
            let mut registers = user_registers([0; GENERAL_REGISTER_COUNT]);
            registers.fs_base = self.thread_pointer;
            Some(registers)
        }

        fn lookup_symbol(&self, _: &str, symbol: &str) -> Option<u64> {
            self.symbols.get(symbol).copied()
        }
    }

    impl Process {
        fn write(&mut self, address: u64, bytes: &[u8]) {
            for (address, byte) in (address..).zip(bytes) {
                self.memory.insert(address, *byte);
            }
        }

        fn word(&mut self, address: u64, value: u64) {
            self.write(address, &value.to_le_bytes());
        }

        fn descriptor(&mut self, name: &'static str, bits: u32, count: u32, offset: i32) {
            let address = DESCRIPTORS + 16 * self.symbols.len() as u64;
            self.symbols.insert(name, address);
            let words = [bits, count, u32::from_le_bytes(offset.to_le_bytes())];
            self.write(address, &words.map(u32::to_le_bytes).concat());
        }

        /// A current DTV whose slot for `module` holds `block`, the module's
        /// static TLS block lying `static_offset` below the thread pointer.
        fn new(module: u64, block: u64, static_offset: u64) -> Self {
            let mut process = Self {
                memory: BTreeMap::new(),
                symbols: BTreeMap::new(),
                thread_pointer: THREAD_POINTER,
            };
            process.descriptor("_thread_db_link_map_l_tls_modid", 64, 1, 0x430);
            process.descriptor("_thread_db_link_map_l_tls_offset", 64, 1, 0x428);
            process.descriptor("_thread_db___nptl_rtld_global", 64, 1, 0);
            process.descriptor(
                "_thread_db_rtld_global__dl_tls_dtv_slotinfo_list",
                64,
                1,
                0x100,
            );
            process.descriptor("_thread_db_dtv_slotinfo_list_len", 64, 1, 0x10);
            process.descriptor("_thread_db_dtv_slotinfo_list_next", 64, 1, 0x8);
            process.descriptor("_thread_db_dtv_slotinfo_list_slotinfo", 192, 0, 0x20);
            process.descriptor("_thread_db_dtv_slotinfo_gen", 64, 1, 0x10);
            process.descriptor("_thread_db_dtv_slotinfo_map", 64, 1, 0);
            process.descriptor("_thread_db_pthread_dtvp", 64, 1, 0x18);
            process.descriptor("_thread_db_dtv_dtv", 128, 0x1000, 0x10);
            process.descriptor("_thread_db_dtv_t_counter", 64, 1, 0);
            process.descriptor("_thread_db_dtv_t_pointer_val", 64, 1, 8);
            process
                .symbols
                .insert("__nptl_rtld_global", NPTL_RTLD_GLOBAL);
            process.word(NPTL_RTLD_GLOBAL, RTLD_GLOBAL);
            process.word(RTLD_GLOBAL + 0x100, FIRST_LIST);
            process.word(FIRST_LIST + 0x10, FIRST_LIST_LENGTH);
            process.word(FIRST_LIST + 0x8, SECOND_LIST);
            process.word(SECOND_LIST + 0x10, FIRST_LIST_LENGTH);
            process.word(SECOND_LIST + 0x8, 0);
            process.word(LINK_MAP + 0x430, module);
            process.word(LINK_MAP + 0x428, static_offset);
            process.slotinfo(module, 2, LINK_MAP);
            process.word(THREAD_POINTER + 0x18, DTV);
            process.generation(2);
            process.word(DTV + 0x10 + 16 * module + 8, block);
            process
        }

        fn slotinfo(&mut self, module: u64, generation: u64, map: u64) {
            let (list, index) = if module < FIRST_LIST_LENGTH {
                (FIRST_LIST, module)
            } else {
                (SECOND_LIST, module - FIRST_LIST_LENGTH)
            };
            let entry = list + 0x20 + 24 * index;
            self.word(entry + 0x10, generation);
            self.word(entry, map);
        }

        fn generation(&mut self, generation: u64) {
            self.word(DTV + 0x10, generation);
        }

        fn address(&self) -> Result<u64, TlsError> {
            tls_address(self, Pid::from_raw(1), LINK_MAP, 0x24)
        }
    }

    #[test]
    fn blocks_follow_the_dtv_and_fall_back_to_static_tls() {
        const BLOCK: u64 = 0x5555_0000;
        let static_block = THREAD_POINTER - 0x80 + 0x24;

        // A current DTV holds an allocated block, in either slotinfo list.
        assert_eq!(Process::new(3, BLOCK, 0x80).address(), Ok(BLOCK + 0x24));
        assert_eq!(Process::new(70, BLOCK, 0x80).address(), Ok(BLOCK + 0x24));

        // An unallocated slot, or a DTV older than the module, leaves only
        // the module's static block.
        let unallocated = Process::new(3, u64::MAX, 0x80);
        assert_eq!(unallocated.address(), Ok(static_block));
        let mut stale = Process::new(3, BLOCK, 0x80);
        stale.generation(1);
        assert_eq!(stale.address(), Ok(static_block));
        for no_static_block in [NO_TLS_OFFSET, FORCED_DYNAMIC_TLS_OFFSET] {
            let mut stale = Process::new(3, BLOCK, no_static_block);
            stale.generation(1);
            assert_eq!(stale.address(), Err(TlsError::Deferred));
            assert_eq!(
                Process::new(3, u64::MAX, no_static_block).address(),
                Err(TlsError::Deferred)
            );
        }

        // A thread before its thread pointer is set, and a module without
        // TLS, have no block.
        let mut early = Process::new(3, BLOCK, 0x80);
        early.thread_pointer = 0;
        assert_eq!(early.address(), Err(TlsError::Deferred));
        assert_eq!(Process::new(0, BLOCK, 0x80).address(), Err(TlsError::NoTls));

        // A statically linked C library holds the slotinfo list itself.
        let mut static_library = Process::new(3, BLOCK, 0x80);
        static_library.symbols.remove("__nptl_rtld_global");
        static_library
            .symbols
            .insert("_dl_tls_dtv_slotinfo_list", 0x2100_0000);
        static_library.word(0x2100_0000, FIRST_LIST);
        static_library.descriptor("_thread_db__dl_tls_dtv_slotinfo_list", 64, 1, 0);
        assert_eq!(static_library.address(), Ok(BLOCK + 0x24));
    }

    #[test]
    fn inconsistent_loader_state_fails_with_its_reason() {
        let failure = |process: &Process| match process.address() {
            Err(TlsError::Failed(message)) => message,
            result => panic!("expected a failure, got {result:?}"),
        };

        let beyond = Process::new(200, 0x5555_0000, 0x80);
        assert!(failure(&beyond).contains("no TLS slot for module 200"));
        let mut cyclic = Process::new(70, 0x5555_0000, 0x80);
        cyclic.word(SECOND_LIST + 0x8, FIRST_LIST);
        cyclic.word(LINK_MAP + 0x430, 300);
        assert!(failure(&cyclic).contains("cycle"));
        let mut overflowing = Process::new(70, 0x5555_0000, 0x80);
        overflowing.word(SECOND_LIST + 0x10, u64::MAX);
        assert!(failure(&overflowing).contains("slotinfo list lengths overflow"));
        let mut unloaded = Process::new(3, 0x5555_0000, 0x80);
        unloaded.slotinfo(3, 2, 0);
        assert!(failure(&unloaded).contains("TLS module 3 is not loaded"));
        // The module's DTV slot must exist in the described array.
        let mut short = Process::new(3, 0x5555_0000, 0x80);
        short.descriptor("_thread_db_dtv_dtv", 128, 3, 0x10);
        assert!(failure(&short).contains("outside _thread_db_dtv_dtv"));

        let mut undescribed = Process::new(3, 0x5555_0000, 0x80);
        undescribed.symbols.remove("_thread_db_pthread_dtvp");
        assert!(failure(&undescribed).contains("does not define _thread_db_pthread_dtvp"));
        let mut empty = Process::new(3, 0x5555_0000, 0x80);
        empty.descriptor("_thread_db_pthread_dtvp", 0, 1, 0x18);
        assert!(failure(&empty).contains("unusable 0-bit field"));
        let mut structure = Process::new(3, 0x5555_0000, 0x80);
        structure.descriptor("_thread_db_dtv_slotinfo_gen", 128, 1, 0x10);
        assert!(failure(&structure).contains("not a value"));
        let mut unreadable = Process::new(3, 0x5555_0000, 0x80);
        unreadable.memory.remove(&(THREAD_POINTER + 0x18));
        assert!(failure(&unreadable).contains("unreadable"));
    }
}
