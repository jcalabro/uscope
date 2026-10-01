//! Fuzzes decoding from known instruction starts over arbitrary bytes,
//! known starts, and unreadable memory.

use super::fake::FakeSource;
use super::{
    AssemblySyntax, BlockCompletion, BoundaryEvidence, DisassemblyBlock, Engine,
    InstructionContent, TargetBoundary, Window, decoder_for,
};
use crate::{
    AddressRange, Architecture, ByteOrder, PointerWidth, TargetDescription, VirtualAddress,
};

/// Places code just below a read-chunk boundary so instructions straddle it.
const BASE: u64 = 0x10_0000 - 0x40;

pub fn run(data: &[u8]) {
    let Some((header, rest)) = data.split_first_chunk::<8>() else {
        return;
    };
    let starts = usize::from(header[0] % 16);
    let Some((starts, code)) = rest.split_at_checked(starts * 2) else {
        return;
    };
    if code.is_empty() {
        return;
    }
    let length = code.len() as u64;
    let offset = |value: u16| u64::from(value) % length;
    let mut source = FakeSource::new(BASE, code.to_vec());
    // An unreadable hole, possibly empty, splits the readable bytes.
    let hole_start = BASE + offset(u16::from_le_bytes([header[1], header[2]]));
    let hole_end = (hole_start + u64::from(header[3] % 32)).min(BASE + length);
    source.readable = vec![
        AddressRange {
            start: BASE,
            end: hole_start,
        },
        AddressRange {
            start: hole_end,
            end: BASE + length,
        },
    ];
    for pair in starts.as_chunks::<2>().0 {
        let evidence = [
            BoundaryEvidence::ProgramCounter,
            BoundaryEvidence::FunctionRange,
            BoundaryEvidence::CodeSymbol,
        ][usize::from(pair[1] % 3)];
        source = source.start(
            BASE + offset(u16::from(pair[0]) << 2 | u16::from(pair[1])),
            evidence,
        );
    }
    let syntax = if header[4] & 1 == 0 {
        AssemblySyntax::Intel
    } else {
        AssemblySyntax::Att
    };
    let target = TargetDescription {
        architecture: Architecture::X86_64,
        byte_order: ByteOrder::Little,
        pointer_width: PointerWidth::Bits64,
    };
    let mut decoder = decoder_for(target, syntax).expect("x86-64 decoder");

    let range = AddressRange {
        start: VirtualAddress::new(BASE + offset(u16::from(header[5]))),
        end: VirtualAddress::new(BASE + length),
    };
    let blocks = Engine::new(&mut source, decoder.as_mut())
        .function(&[range])
        .expect("fake reads never fail");
    let [block] = blocks.as_slice() else {
        panic!("one range yields one block");
    };
    check_block(&source, block, true);

    let address = VirtualAddress::new(BASE + offset(u16::from(header[6]) << 4));
    let before = u32::from(header[7] % 16);
    let after = u32::from(header[7] / 16);
    let window = Engine::new(&mut source, decoder.as_mut())
        .window(address, before, after)
        .expect("fake reads never fail");
    check_block(&source, &window.block, false);
    check_window(&source, address, before, after, &window);
}

/// Checks that instructions are ordered, faithful to memory, and tile their
/// range except where a reported conflict resumes decoding at a known start.
fn check_block(source: &FakeSource, block: &DisassemblyBlock, bounded: bool) {
    let instructions = &block.instructions;
    for (index, instruction) in instructions.iter().enumerate() {
        let length = instruction.bytes.len();
        assert!((1..=15).contains(&length), "{instruction:?}");
        for (offset, byte) in instruction.bytes.iter().enumerate() {
            assert_eq!(
                source.byte(instruction.address.get() + offset as u64),
                Some(*byte),
                "{instruction:?}"
            );
        }
        match instruction.content {
            InstructionContent::Invalid => assert_eq!(length, 1),
            InstructionContent::Truncated => {
                let end = instruction.end().get();
                assert_eq!(source.byte(end), None, "truncation needs unreadable memory");
                // Only a known start inside the truncated bytes resumes
                // decoding; otherwise truncation ends the block.
                let resumes = block
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.instruction == instruction.address);
                assert!(resumes || index + 1 == instructions.len());
                if !resumes {
                    assert!(matches!(
                        block.completion,
                        BlockCompletion::Unreadable { address, .. } if address.get() == end
                    ));
                }
            }
            InstructionContent::Decoded(_) => {}
        }
        let Some(next) = instructions.get(index + 1) else {
            continue;
        };
        if next.address == instruction.end() {
            continue;
        }
        assert!(
            block.conflicts.iter().any(|conflict| {
                conflict.instruction == instruction.address && conflict.boundary == next.address
            }),
            "unexplained gap after {instruction:?}: {block:#?}"
        );
        assert!(instruction.address < next.address && next.address < instruction.end());
    }
    for conflict in block.conflicts.iter() {
        if conflict.evidence == BoundaryEvidence::RangeEnd {
            assert_eq!(conflict.boundary, block.range.end);
            continue;
        }
        // Decoding resumes at the boundary unless the conflicting
        // instruction ends the block: the boundary is unreadable or the
        // requested count is reached.
        let resumed = instructions
            .iter()
            .any(|instruction| instruction.address == conflict.boundary);
        let last = instructions.last().map(|last| last.address) == Some(conflict.instruction);
        assert!(
            resumed || last,
            "decoding resumes at {conflict:?}: {block:#?}"
        );
        if !resumed && let BlockCompletion::Unreadable { address, .. } = block.completion {
            assert_eq!(address, conflict.boundary);
        }
    }
    if bounded {
        // Every known start inside the range begins an instruction unless
        // decoding stopped before reaching it.
        let decoded_end = instructions
            .last()
            .map_or(block.range.start, super::DisassembledInstruction::end);
        for start in source.starts.keys() {
            if block.range.start <= *start && *start < decoded_end.min(block.range.end) {
                assert!(
                    instructions
                        .iter()
                        .any(|instruction| instruction.address == *start),
                    "known start {start} was skipped: {block:#?}"
                );
            }
        }
        if block.completion == BlockCompletion::Complete {
            let crossed = block
                .conflicts
                .iter()
                .any(|conflict| conflict.evidence == BoundaryEvidence::RangeEnd);
            assert!(crossed || decoded_end == block.range.end, "{block:#?}");
        }
    }
}

fn check_window(
    source: &FakeSource,
    address: VirtualAddress,
    before: u32,
    after: u32,
    window: &Window,
) {
    let (boundary, shortfall, block) = (window.boundary, window.shortfall, &window.block);
    let leading = block
        .instructions
        .iter()
        .filter(|instruction| instruction.address < address)
        .collect::<Vec<_>>();
    assert!(leading.len() <= before as usize);
    assert_eq!(shortfall.is_some(), leading.len() < before as usize);
    // Leading instructions are contiguous and end exactly at the address.
    if let Some(last) = leading.last() {
        assert_eq!(last.end(), address, "{block:#?}");
        assert!(
            leading
                .windows(2)
                .all(|pair| pair[0].end() == pair[1].address)
        );
        assert!(matches!(
            boundary,
            TargetBoundary::Known(_) | TargetBoundary::Reached { .. }
        ));
    }
    match boundary {
        TargetBoundary::Known(_) => assert!(source.starts.contains_key(&address)),
        TargetBoundary::Reached { from, .. } | TargetBoundary::Crossed { from, .. } => {
            assert!(source.starts.contains_key(&from) && from < address);
            assert!(!source.starts.contains_key(&address));
        }
        TargetBoundary::Unverified(_) => assert!(!source.starts.contains_key(&address)),
    }
    if let TargetBoundary::Crossed { instruction, .. } = boundary {
        assert!(instruction < address);
    }
    if after != 0 && source.byte(address.get()).is_some() {
        assert_eq!(
            block
                .instructions
                .iter()
                .find(|instruction| instruction.address >= address)
                .map(|instruction| instruction.address),
            Some(address)
        );
    }
}
