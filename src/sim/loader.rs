//! Loads golden executables into simulated address spaces as Linux's ELF
//! loader does for a static executable with address randomization off.
//!
//! An [`Image`] is prepared once per binary and shared by every session
//! that runs it; [`Image::load`] gives a session its own address space,
//! sharing the image's pages until the session writes them, and builds the
//! initial stack from the session's arguments.

use std::sync::Arc;

use object::{Object as _, ObjectSegment as _, SegmentFlags};

use super::cpu::{RSP, Registers};
use super::memory::{AddressSpace, Backing, PAGE_BYTES, PAGE_SIZE, Page, Protection};

/// The top of the stack of a process run without address randomization.
pub const STACK_TOP: u64 = 0x7fff_ffff_f000;
/// The size of the stack mapping a new process starts with.
const STACK_SIZE: u64 = 132 * 1024;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// The `rflags` a new process starts with: interrupts enabled and the
/// reserved bit that always reads as one.
pub const INITIAL_RFLAGS: u64 = 0x202;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("not a loadable ELF executable: {0}")]
    Object(#[from] object::Error),
    #[error("{0}")]
    Unsupported(String),
}

struct Segment {
    start: u64,
    end: u64,
    protection: Protection,
    offset: u64,
    /// Page contents from `start`, one per page.
    pages: Vec<Arc<Page>>,
}

/// A static executable prepared for loading.
pub struct Image {
    path: Arc<str>,
    inode: u64,
    entry: u64,
    program_headers: u64,
    program_header_count: u64,
    segments: Vec<Segment>,
}

impl Image {
    /// Prepares an executable that the simulated kernel names `path` and
    /// whose file has `inode`.
    pub fn new(path: &str, inode: u64, data: &[u8]) -> Result<Self, LoadError> {
        let file = object::File::parse(data)?;
        let object::File::Elf64(elf) = &file else {
            return Err(LoadError::Unsupported("not a 64-bit ELF file".into()));
        };
        if file.kind() != object::ObjectKind::Executable {
            return Err(LoadError::Unsupported(
                "only static, position-dependent executables load".into(),
            ));
        }
        let header = elf.elf_header();
        let endian = elf.endian();
        let program_header_offset = header.e_phoff.get(endian);
        let program_header_count = u64::from(header.e_phnum.get(endian));

        let mut segments = Vec::new();
        let mut program_headers = None;
        for segment in file.segments() {
            let SegmentFlags::Elf { p_flags } = segment.flags() else {
                return Err(LoadError::Unsupported("segment without ELF flags".into()));
            };
            let (file_offset, file_size) = segment.file_range();
            let address = segment.address();
            if address % PAGE_SIZE != file_offset % PAGE_SIZE {
                return Err(LoadError::Unsupported("misaligned segment".into()));
            }
            if (file_offset..file_offset + file_size).contains(&program_header_offset) {
                program_headers = Some(address + program_header_offset - file_offset);
            }
            segments.push(prepare_segment(
                data,
                address,
                segment.size(),
                file_offset,
                file_size,
                p_flags,
            )?);
        }
        Ok(Self {
            path: Arc::from(path),
            inode,
            entry: file.entry(),
            program_headers: program_headers
                .ok_or_else(|| LoadError::Unsupported("program headers are not loaded".into()))?,
            program_header_count,
            segments,
        })
    }

    #[must_use]
    pub const fn entry(&self) -> u64 {
        self.entry
    }

    /// Every executable page the image maps, with its address, as loaded.
    pub fn code(&self) -> impl Iterator<Item = (u64, &Arc<Page>)> + '_ {
        self.segments
            .iter()
            .filter(|segment| segment.protection.execute)
            .flat_map(|segment| {
                segment
                    .pages
                    .iter()
                    .enumerate()
                    .map(move |(index, page)| (segment.start + index as u64 * PAGE_SIZE, page))
            })
    }

    /// A new address space running the image with `arguments` after the
    /// program's name, and the registers it starts with. `random` fills
    /// the sixteen bytes `AT_RANDOM` names.
    #[must_use]
    pub fn load(&self, arguments: &[String], random: [u8; 16]) -> (AddressSpace, Registers) {
        let mut space = AddressSpace::default();
        for segment in &self.segments {
            let backing = Backing::File {
                path: Arc::clone(&self.path),
                inode: self.inode,
                offset: segment.offset,
            };
            space.map(segment.start, segment.end, segment.protection, backing);
            for (index, page) in segment.pages.iter().enumerate() {
                space.share_page(segment.start + index as u64 * PAGE_SIZE, Arc::clone(page));
            }
        }
        space.map(
            STACK_TOP - STACK_SIZE,
            STACK_TOP,
            Protection::READ_WRITE,
            Backing::Stack,
        );
        let stack_pointer = self.build_stack(&mut space, arguments, random);
        let mut registers = Registers {
            rip: self.entry,
            rflags: INITIAL_RFLAGS,
            ..Registers::default()
        };
        registers.general[RSP] = stack_pointer;
        (space, registers)
    }

    /// Lays out the initial stack as Linux does, top down: the executable's
    /// name, the argument strings, the random bytes, then argc, argv, an
    /// empty environment, and the auxiliary vector, with argc 16-byte
    /// aligned. Returns the stack pointer, which points at argc.
    fn build_stack(&self, space: &mut AddressSpace, arguments: &[String], random: [u8; 16]) -> u64 {
        let mut top = STACK_TOP - 8;
        let mut push_bytes = |space: &mut AddressSpace, bytes: &[u8]| {
            top -= bytes.len() as u64;
            assert!(space.write(top, bytes).is_ok(), "the stack is writable");
            top
        };
        let terminated = |text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            bytes
        };
        let name = terminated(&self.path);
        let executable_name = push_bytes(space, &name);
        let mut argv = vec![push_bytes(space, &name)];
        for argument in arguments {
            argv.push(push_bytes(space, &terminated(argument)));
        }
        let random_address = push_bytes(space, &random);

        let mut words = vec![argv.len() as u64];
        words.extend(&argv);
        words.extend([0, 0]);
        words.extend([
            AT_PHDR,
            self.program_headers,
            AT_PHENT,
            56,
            AT_PHNUM,
            self.program_header_count,
            AT_PAGESZ,
            PAGE_SIZE,
            AT_ENTRY,
            self.entry,
            AT_RANDOM,
            random_address,
            AT_EXECFN,
            executable_name,
            AT_NULL,
            0,
        ]);
        let size = words.len() as u64 * 8;
        let stack_pointer = (top - size) & !15;
        let bytes = words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>();
        assert!(
            space.write(stack_pointer, &bytes).is_ok(),
            "the stack is writable"
        );
        stack_pointer
    }
}

/// Reads one segment's pages as the kernel maps them: whole pages of the
/// file from the segment's page-aligned offset, with a writable segment's
/// bytes past its file contents zeroed.
fn prepare_segment(
    data: &[u8],
    address: u64,
    memory_size: u64,
    file_offset: u64,
    file_size: u64,
    flags: u32,
) -> Result<Segment, LoadError> {
    let protection = Protection {
        read: flags & object::elf::PF_R != 0,
        write: flags & object::elf::PF_W != 0,
        execute: flags & object::elf::PF_X != 0,
    };
    let start = address & !(PAGE_SIZE - 1);
    let end = (address + memory_size).next_multiple_of(PAGE_SIZE);
    let offset = file_offset & !(PAGE_SIZE - 1);
    let file_end = address + file_size;
    let mut pages = Vec::new();
    for page_address in (start..end).step_by(PAGE_BYTES) {
        let mut page = [0; PAGE_BYTES];
        if page_address < file_end {
            let from = usize::try_from(offset + (page_address - start))
                .map_err(|_| LoadError::Unsupported("segment offset overflows".into()))?;
            let available = data.len().saturating_sub(from).min(page.len());
            page[..available].copy_from_slice(&data[from..from + available]);
            if protection.write && page_address + PAGE_SIZE > file_end {
                let keep = usize::try_from(file_end - page_address).expect("within one page");
                page[keep..].fill(0);
            }
        }
        pages.push(Arc::new(page));
    }
    Ok(Segment {
        start,
        end,
        protection,
        offset,
        pages,
    })
}
