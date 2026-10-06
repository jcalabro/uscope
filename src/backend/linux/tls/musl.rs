//! Thread-local storage addresses in musl's thread layout.
//!
//! musl has no thread debugging library, but the start of its thread
//! descriptor is ABI that its own assembly depends on. On x86-64 the thread
//! pointer addresses the descriptor, whose first word points back at the
//! descriptor and whose second points at the thread's dynamic thread vector
//! (DTV). The DTV's first word counts the modules it has slots for, and slot
//! N holds the address of TLS module N's block.
//!
//! musl gives a thread every module's block before the module's code can
//! run: a new thread copies every block, and `dlopen` extends every thread's
//! DTV before it runs the library's code. A DTV with too few slots belongs to
//! a thread caught in between, or to a musl older than 1.1.22, which gave
//! threads a `dlopen`ed library's block only when they first used it.

use std::num::NonZeroU64;

use nix::unistd::Pid;

use super::{ProcessServices, TlsError, failed};

/// Where the descriptor keeps its DTV's address, after its own.
const DTV_OFFSET: u64 = 8;
const WORD_SIZE: u64 = 8;

/// The address of `offset` within the TLS block of module `module` for
/// `thread`.
pub(super) fn tls_address(
    services: &dyn ProcessServices,
    thread: Pid,
    module: NonZeroU64,
    offset: u64,
) -> Result<u64, TlsError> {
    let thread_pointer = services
        .registers(thread)
        .ok_or_else(|| failed(format!("the registers of thread {thread} are unavailable")))?
        .fs_base;
    // A thread has no thread pointer until musl sets one up.
    if thread_pointer == 0 {
        return Err(TlsError::Deferred);
    }
    let read = |address: Option<u64>| {
        let address = address.ok_or_else(|| failed("musl's thread state overflows"))?;
        let mut bytes = [0; 8];
        if services.read(address, &mut bytes) {
            Ok(u64::from_le_bytes(bytes))
        } else {
            Err(failed(format!(
                "musl's thread state at {address:#x} is unreadable"
            )))
        }
    };
    if read(Some(thread_pointer))? != thread_pointer {
        return Err(failed(format!(
            "the thread pointer {thread_pointer:#x} does not address a musl thread descriptor"
        )));
    }
    let dtv = read(thread_pointer.checked_add(DTV_OFFSET))?;
    if module.get() > read(Some(dtv))? {
        return Err(TlsError::Deferred);
    }
    let block = read(
        module
            .get()
            .checked_mul(WORD_SIZE)
            .and_then(|slot| dtv.checked_add(slot)),
    )?;
    if block == 0 {
        return Err(TlsError::Deferred);
    }
    block
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

    struct Process {
        words: BTreeMap<u64, u64>,
        thread_pointer: u64,
    }

    impl ProcessServices for Process {
        fn read(&self, address: u64, output: &mut [u8]) -> bool {
            let Some(word) = self.words.get(&address) else {
                return false;
            };
            output.copy_from_slice(&word.to_le_bytes()[..output.len()]);
            true
        }

        fn registers(&self, _: Pid) -> Option<libc::user_regs_struct> {
            let mut registers = user_registers([0; GENERAL_REGISTER_COUNT]);
            registers.fs_base = self.thread_pointer;
            Some(registers)
        }

        fn lookup_symbol(&self, _: &str, _: &str) -> Option<u64> {
            None
        }
    }

    impl Process {
        /// A thread whose DTV has a slot for each of `blocks`.
        fn new(blocks: &[u64]) -> Self {
            let mut words = BTreeMap::from([
                (THREAD_POINTER, THREAD_POINTER),
                (THREAD_POINTER + 8, DTV),
                (DTV, blocks.len() as u64),
            ]);
            for (slot, block) in (1..).zip(blocks) {
                words.insert(DTV + 8 * slot, *block);
            }
            Self {
                words,
                thread_pointer: THREAD_POINTER,
            }
        }

        fn address(&self, module: u64) -> Result<u64, TlsError> {
            let module = NonZeroU64::new(module).expect("module numbers start at one");
            tls_address(self, Pid::from_raw(1), module, 0x24)
        }
    }

    #[test]
    fn blocks_are_found_through_the_dtv_unless_the_thread_lacks_them() {
        let process = Process::new(&[0x1000, 0x2000, 0x3000]);
        assert_eq!(process.address(1), Ok(0x1024));
        assert_eq!(process.address(3), Ok(0x3024));

        // A module loaded after the thread's DTV was last extended, an empty
        // slot, and a thread before its thread pointer is set have no block.
        assert_eq!(process.address(4), Err(TlsError::Deferred));
        assert_eq!(
            Process::new(&[0x1000, 0]).address(2),
            Err(TlsError::Deferred)
        );
        let mut early = Process::new(&[0x1000]);
        early.thread_pointer = 0;
        assert_eq!(early.address(1), Err(TlsError::Deferred));

        let failure = |process: &Process| match process.address(1) {
            Err(TlsError::Failed(message)) => message,
            result => panic!("expected a failure, got {result:?}"),
        };
        // A thread pointer that does not address a descriptor of its own is
        // not musl's.
        let mut foreign = Process::new(&[0x1000]);
        foreign.words.insert(THREAD_POINTER, 0);
        assert!(failure(&foreign).contains("does not address a musl thread descriptor"));
        let mut unreadable = Process::new(&[0x1000]);
        unreadable.words.remove(&DTV);
        assert!(failure(&unreadable).contains("0x60000000 is unreadable"));
        let mut overflowing = Process::new(&[0x1000]);
        overflowing.words.insert(DTV + 8, u64::MAX);
        assert!(failure(&overflowing).contains("TLS address overflows"));
    }
}
