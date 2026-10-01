mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;

use object::{Object, ObjectSection};
use support::Scenario;
use uscope::{
    AssemblySyntax, BlockCompletion, BoundaryConflict, BoundaryEvidence, BreakpointSpec,
    ContextShortfall, ControlFlow, CoreDumpOptions, CoreModuleState, DisassembledInstruction,
    Disassembly, DisassemblyBlock, DisassemblyQuery, DisassemblyRange, DisassemblyView, Error,
    FunctionOrigin, ImageAddress, InstructionContent, InstructionReferenceKind, LoadedModuleRecord,
    MAX_WINDOW_AFTER, MAX_WINDOW_BEFORE, ModuleImage, StopReason, SymbolExtentProvenance,
    SymbolKind, TargetBoundary, VirtualAddress,
};

/// GNU objdump's decoding of one ELF file, by image address.
struct Objdump {
    instructions: BTreeMap<u64, ObjdumpInstruction>,
}

#[derive(Debug)]
struct ObjdumpInstruction {
    bytes: Vec<u8>,
    text: String,
}

impl Objdump {
    fn read(file_name: &str) -> Self {
        let path = Scenario::fixture(&format!("disassembly-oracles/{file_name}.objdump"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let mut instructions = BTreeMap::new();
        for line in text.lines() {
            // "  ADDRESS:\tBYTES\tTEXT", where a wide listing pads BYTES.
            let mut fields = line.split('\t');
            let (Some(address), Some(bytes)) = (fields.next(), fields.next()) else {
                continue;
            };
            let Some(address) = address
                .trim()
                .strip_suffix(':')
                .and_then(|address| u64::from_str_radix(address, 16).ok())
            else {
                continue;
            };
            let bytes = bytes
                .split_whitespace()
                .map(|byte| u8::from_str_radix(byte, 16).expect("objdump byte"))
                .collect();
            let text = fields.next().unwrap_or_default().trim().to_owned();
            instructions.insert(address, ObjdumpInstruction { bytes, text });
        }
        assert!(!instructions.is_empty(), "{} is empty", path.display());
        Self { instructions }
    }
}

impl ObjdumpInstruction {
    /// The target of a direct jump or call, which objdump prints as a bare
    /// hexadecimal operand.
    fn branch_target(&self) -> Option<u64> {
        const PREFIXES: [&str; 7] = ["bnd", "notrack", "rep", "repz", "repnz", "data16", "addr32"];
        let mut tokens = self
            .text
            .split_whitespace()
            .skip_while(|token| PREFIXES.contains(token) || token.starts_with("rex"));
        let mnemonic = tokens.next()?;
        let branch = mnemonic.starts_with('j')
            || mnemonic.starts_with("call")
            || mnemonic.starts_with("loop")
            || mnemonic.starts_with("xbegin");
        let operand = tokens.next()?;
        branch
            .then(|| u64::from_str_radix(operand, 16).ok())
            .flatten()
    }

    /// The address objdump computes for a program-counter-relative operand,
    /// which it prints after `#`.
    fn computed_address(&self) -> Option<u64> {
        let (_, comment) = self.text.split_once('#')?;
        let address = comment.split_whitespace().next()?;
        u64::from_str_radix(address.trim_start_matches("0x"), 16).ok()
    }
}

const fn query(range: DisassemblyRange) -> DisassemblyQuery {
    DisassemblyQuery {
        range,
        syntax: AssemblySyntax::Att,
    }
}

const fn window(address: u64, before: u32, after: u32) -> DisassemblyQuery {
    query(DisassemblyRange::Window {
        address: VirtualAddress::new(address),
        before,
        after,
    })
}

async fn disassemble(scenario: &Scenario, query: DisassemblyQuery) -> Disassembly {
    scenario
        .operation("disassemble", scenario.handle().disassemble(query))
        .await
}

fn into_window(
    disassembly: Disassembly,
) -> (TargetBoundary, Option<ContextShortfall>, DisassemblyBlock) {
    match disassembly.view {
        DisassemblyView::Window {
            boundary,
            leading,
            block,
            ..
        } => (boundary, leading, block),
        DisassemblyView::Function { .. } => panic!("a window query returned a function"),
    }
}

fn decoded(instruction: &DisassembledInstruction) -> &uscope::DecodedInstruction {
    match &instruction.content {
        InstructionContent::Decoded(decoded) => decoded,
        other => panic!("{:#x} did not decode: {other:?}", instruction.address),
    }
}

/// The modules of one stopped process with their images.
struct Modules {
    records: Vec<LoadedModuleRecord>,
    images: BTreeMap<uscope::ModuleId, Arc<ModuleImage>>,
}

impl Modules {
    async fn load(scenario: &Scenario) -> Self {
        let snapshot = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        let mut images = BTreeMap::new();
        for record in snapshot.modules.iter() {
            let image = scenario
                .operation(
                    "image",
                    scenario.handle().loaded_module_image(record.module.id),
                )
                .await;
            images.insert(record.module.id, image);
        }
        Self {
            records: snapshot.modules.to_vec(),
            images,
        }
    }

    fn named(&self, file_name: &str) -> (&LoadedModuleRecord, &ModuleImage) {
        let record = self
            .records
            .iter()
            .find(|record| {
                record
                    .path
                    .file_name()
                    .is_some_and(|name| name == file_name)
            })
            .unwrap_or_else(|| panic!("no loaded module named {file_name}"));
        (record, &self.images[&record.module.id])
    }

    /// Returns the runtime address of a uniquely named symbol.
    fn symbol(&self, file_name: &str, name: &str) -> u64 {
        let (record, image) = self.named(file_name);
        let symbol = image
            .symbols()
            .iter()
            .find(|symbol| symbol.name.as_ref() == name)
            .unwrap_or_else(|| panic!("{file_name} has no symbol {name}"));
        record.module.load_bias + symbol.address.get()
    }
}

/// Pages through every executable section of a module with windows that
/// each begin where the last ended, then compares every instruction with
/// objdump: the same instruction starts, the same bytes, and the same
/// encoded addresses. No known instruction start may conflict.
async fn assert_module_matches_objdump(
    scenario: &Scenario,
    modules: &Modules,
    file_name: &str,
    context: &str,
) -> usize {
    let (record, image) = modules.named(file_name);
    let bias = record.module.load_bias;
    let oracle = Objdump::read(file_name);
    let mut compared = 0;
    for section in image.sections().iter().filter(|section| section.executable) {
        let (start, end) = (section.range.start.get(), section.range.end.get());
        let context = format!("{context}: {file_name} {}", section.name);
        let mut decoded = BTreeMap::new();
        let mut address = start;
        while address < end {
            let (boundary, _, block) = into_window(
                disassemble(scenario, window(bias + address, 0, MAX_WINDOW_AFTER)).await,
            );
            assert!(
                !matches!(boundary, TargetBoundary::Crossed { .. }),
                "{context}: {address:#x} is inside an instruction: {boundary:?}"
            );
            // Decoding past the section's end may cross padding into the next
            // section; only conflicts inside the section matter.
            let conflicts = block
                .conflicts
                .iter()
                .filter(|conflict| conflict.boundary.get() - bias < end)
                .map(|conflict| {
                    (
                        conflict.boundary.get() - bias,
                        conflict.evidence,
                        conflict.instruction.get() - bias,
                    )
                })
                .collect::<Vec<_>>();
            assert!(conflicts.is_empty(), "{context}: {conflicts:x?}");
            for instruction in block.instructions.iter() {
                let image_address = instruction.address.get() - bias;
                if image_address >= end {
                    break;
                }
                decoded.insert(image_address, instruction.clone());
                address = instruction.end().get() - bias;
            }
            if let BlockCompletion::Unreadable { address: stop, .. } = block.completion {
                assert!(stop.get() - bias >= end, "{context}: unreadable at {stop}");
            }
        }

        let expected = oracle
            .instructions
            .range(start..end)
            .collect::<BTreeMap<_, _>>();
        let actual = decoded.keys().collect::<BTreeSet<_>>();
        let missing = expected
            .keys()
            .filter(|address| !actual.contains(*address))
            .take(4)
            .collect::<Vec<_>>();
        let invented = actual
            .iter()
            .filter(|address| !expected.contains_key(**address))
            .take(4)
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty() && invented.is_empty(),
            "{context}: objdump-only starts {missing:x?}, uscope-only starts {invented:x?}"
        );
        for (address, instruction) in &decoded {
            let objdump = expected[address];
            let context = format!("{context}+{address:#x} {objdump:?}");
            assert_eq!(*instruction.bytes, *objdump.bytes, "{context}");
            assert_eq!(
                matches!(instruction.content, InstructionContent::Invalid),
                objdump.text.starts_with("(bad)"),
                "{context}"
            );
            let InstructionContent::Decoded(decoded) = &instruction.content else {
                continue;
            };
            let targets = |kind| {
                decoded
                    .references
                    .iter()
                    .filter(move |reference| reference.kind == kind)
                    .map(|reference| reference.address.get() - bias)
                    .collect::<Vec<_>>()
            };
            let branches = targets(InstructionReferenceKind::BranchTarget);
            assert_eq!(
                branches.first().copied(),
                objdump.branch_target(),
                "{context}"
            );
            if let Some(computed) = objdump.computed_address() {
                assert!(
                    targets(InstructionReferenceKind::MemoryOperand).contains(&computed),
                    "{context}: {decoded:?}"
                );
            }
            compared += 1;
        }
    }
    assert!(compared > 0, "{context}: {file_name} had no code");
    compared
}

/// Launches a fixture that faults, installs breakpoints at code symbols so
/// that traps are present in the compared code, and returns at the fault.
async fn faulted_with_breakpoints(fixture: &str) -> (Scenario, Modules) {
    let mut scenario = Scenario::launch(fixture);
    let stop = scenario.run_to_stop().await;
    assert!(
        matches!(stop, StopReason::Exception(_)),
        "{fixture}: {stop:?}"
    );
    let modules = Modules::load(&scenario).await;
    let (record, image) = modules.named(fixture);
    let starts = image
        .symbols()
        .iter()
        .filter_map(|symbol| symbol.extent)
        .map(|extent| extent.range.start.get())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let bias = record.module.load_bias;
    for address in starts.iter().step_by(starts.len().div_ceil(24).max(1)) {
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(bias + address)))
            .await;
    }
    (scenario, modules)
}

#[tokio::test]
async fn c_programs_and_libraries_decode_like_objdump() {
    for fixture in ["crash-gcc-o0", "crash-gcc-o2-nopie", "crash-clang-o2"] {
        let (scenario, modules) = faulted_with_breakpoints(fixture).await;
        assert_module_matches_objdump(&scenario, &modules, fixture, fixture).await;
        assert_module_matches_objdump(&scenario, &modules, "libcrash.so", fixture).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn the_c_library_and_loader_decode_like_objdump() {
    let (scenario, modules) = faulted_with_breakpoints("crash-gcc-o0").await;
    for library in ["libc.so.6", "ld-linux-x86-64.so.2"] {
        let compared = assert_module_matches_objdump(&scenario, &modules, library, "live").await;
        assert!(compared > 10_000, "{library}: {compared}");
    }
    scenario.shutdown().await;
}

#[tokio::test]
async fn rust_zig_and_go_programs_decode_like_objdump() {
    for fixture in ["crash-rust-o0", "crash-zig-o0", "crash-go-o0"] {
        let (scenario, modules) = faulted_with_breakpoints(fixture).await;
        assert_module_matches_objdump(&scenario, &modules, fixture, fixture).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn core_dumps_decode_verified_files_like_objdump() {
    for (core, files) in [
        (
            "crash-gcc-o0-segv.core",
            &["crash-gcc-o0", "libcrash.so", "libc.so.6"][..],
        ),
        (
            "elf-symbols-gcc-o0.core",
            &["elf-symbols-gcc-o0", "libelf-symbols-gcc.so"][..],
        ),
        (
            "elf-symbols-stripped.core",
            &["libelf-symbols-stripped.so"][..],
        ),
    ] {
        let scenario = Scenario::open_core(core, &CoreDumpOptions::new(Scenario::fixture(core)));
        let modules = Modules::load(&scenario).await;
        for file in files {
            assert_module_matches_objdump(&scenario, &modules, file, core).await;
        }
        scenario.shutdown().await;
    }
}

/// Checks one function disassembly against its image and objdump: one block
/// per range, every instruction as objdump decodes it, and no conflicts.
fn assert_function_blocks(
    disassembly: &Disassembly,
    ranges: &[(u64, u64)],
    bias: u64,
    oracle: &Objdump,
    context: &str,
) {
    let DisassemblyView::Function { blocks, .. } = &disassembly.view else {
        panic!("{context}: a function query returned a window");
    };
    let actual = blocks
        .iter()
        .map(|block| (block.range.start.get() - bias, block.range.end.get() - bias))
        .collect::<Vec<_>>();
    assert_eq!(actual, ranges, "{context}");
    for block in blocks.iter() {
        assert_eq!(block.completion, BlockCompletion::Complete, "{context}");
        assert!(
            block.conflicts.is_empty(),
            "{context}: {:?}",
            block.conflicts
        );
        let mut next = block.range.start;
        for instruction in block.instructions.iter() {
            assert_eq!(instruction.address, next, "{context}");
            let image_address = instruction.address.get() - bias;
            let objdump = &oracle.instructions[&image_address];
            assert_eq!(
                *instruction.bytes, *objdump.bytes,
                "{context}+{image_address:#x}"
            );
            next = instruction.end();
        }
        assert_eq!(next, block.range.end, "{context}");
    }
}

#[tokio::test]
async fn functions_cover_every_debug_information_range() {
    for fixture in [
        "crash-gcc-o0",
        "crash-gcc-o2-nopie",
        "crash-clang-o2",
        "crash-rust-o0",
    ] {
        let (scenario, modules) = faulted_with_breakpoints(fixture).await;
        let (record, image) = modules.named(fixture);
        let bias = record.module.load_bias;
        let oracle = Objdump::read(fixture);
        let mut split = 0;
        for instance in image
            .code_instances()
            .iter()
            .filter(|instance| matches!(instance.kind, uscope::CodeInstanceKind::OutOfLine))
        {
            let mut ranges = instance
                .ranges
                .iter()
                .map(|range| (range.start.get(), range.end.get()))
                .collect::<Vec<_>>();
            ranges.sort_unstable();
            // Every range resolves to the same function.
            for (start, _) in &ranges {
                let disassembly = disassemble(
                    &scenario,
                    query(DisassemblyRange::Function(VirtualAddress::new(
                        bias + start,
                    ))),
                )
                .await;
                let context = format!("{fixture}: instance {}", instance.id);
                assert_function_blocks(&disassembly, &ranges, bias, &oracle, &context);
                let DisassemblyView::Function { function, .. } = &disassembly.view else {
                    unreachable!();
                };
                assert_eq!(function.module, record.module.id);
                assert_eq!(
                    function.origin,
                    FunctionOrigin::DebugInfo {
                        instance: instance.id
                    },
                    "{context}"
                );
            }
            split += usize::from(ranges.len() > 1);
        }
        if fixture == "crash-gcc-o2-nopie" {
            assert!(
                split > 0,
                "the optimized build splits a cold path from main"
            );
        }
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn split_functions_name_each_range_by_its_own_symbol() {
    let (scenario, modules) = faulted_with_breakpoints("crash-gcc-o2-nopie").await;
    let main = modules.symbol("crash-gcc-o2-nopie", "main");
    let disassembly = disassemble(
        &scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(main))),
    )
    .await;
    let DisassemblyView::Function { function, blocks } = &disassembly.view else {
        panic!("not a function");
    };
    assert_eq!(function.name.as_ref(), "main");
    let names = blocks
        .iter()
        .map(|block| {
            block.instructions[0]
                .location
                .module
                .as_ref()
                .and_then(|module| module.image.symbol.as_ref())
                .map(|symbol| (symbol.name.to_string(), symbol.offset))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            Some(("main.cold".to_owned(), 0)),
            Some(("main".to_owned(), 0))
        ]
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn functions_known_only_by_symbols_use_their_extents() {
    let scenario = Scenario::open_core(
        "elf-symbols-gcc-o0.core",
        &CoreDumpOptions::new(Scenario::fixture("elf-symbols-gcc-o0.core")),
    );
    let modules = Modules::load(&scenario).await;
    let library = "libelf-symbols-gcc.so";
    let (record, image) = modules.named(library);
    let bias = record.module.load_bias;
    let oracle = Objdump::read(library);
    let mut origins = BTreeSet::new();
    for symbol in image.symbols() {
        let Some(extent) = symbol.extent else {
            continue;
        };
        let address = VirtualAddress::new(bias + extent.range.start.get());
        let disassembly = disassemble(&scenario, query(DisassemblyRange::Function(address))).await;
        let DisassemblyView::Function { function, .. } = &disassembly.view else {
            panic!("not a function");
        };
        // The preferred symbol at the start may be an alias or an
        // enclosing function.
        let named = image
            .symbolize(extent.range.start)
            .expect("code at a symbol is named");
        let named_extent = image
            .symbol(named.symbol)
            .and_then(|info| info.extent)
            .expect("extent");
        let context = format!("{library}: {}", symbol.name);
        assert_eq!(
            function.origin,
            FunctionOrigin::Symbol {
                symbol: named.symbol,
                provenance: named.provenance,
            },
            "{context}"
        );
        assert_function_blocks(
            &disassembly,
            &[(named_extent.range.start.get(), named_extent.range.end.get())],
            bias,
            &oracle,
            &context,
        );
        origins.insert((named.name.to_string(), named.provenance));
    }
    for expected in [
        ("asm_nested_outer", SymbolExtentProvenance::Declared),
        ("asm_unsized", SymbolExtentProvenance::Inferred),
        ("lib_fault", SymbolExtentProvenance::Declared),
    ] {
        assert!(
            origins.contains(&(expected.0.to_owned(), expected.1)),
            "{expected:?} not in {origins:?}"
        );
    }

    // Code that no symbol or debug information names has no function.
    let text = image
        .sections()
        .iter()
        .find(|section| section.name.as_ref() == ".text")
        .expect("text")
        .range;
    let unnamed = (text.start.get()..text.end.get())
        .find(|address| image.symbolize(ImageAddress::new(*address)).is_none())
        .expect("unnamed code");
    let missing = VirtualAddress::new(bias + unnamed);
    assert!(matches!(
        scenario
            .handle()
            .disassemble(query(DisassemblyRange::Function(missing)))
            .await,
        Err(Error::NoFunctionContainsAddress(address)) if address == missing
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn context_before_an_address_is_proven_from_known_starts() {
    let (scenario, modules) = faulted_with_breakpoints("crash-gcc-o0").await;
    let oracle = Objdump::read("crash-gcc-o0");
    let (record, _) = modules.named("crash-gcc-o0");
    let bias = record.module.load_bias;
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    let pc = location.address.get();

    // The program counter is itself proven, and every leading instruction
    // is reached by decoding forward from a known start.
    let (boundary, leading, block) = into_window(disassemble(&scenario, window(pc, 16, 4)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
    );
    assert_eq!(leading, None);
    assert_eq!(block.instructions.len(), 20);
    assert_eq!(block.instructions[16].address.get(), pc);
    for pair in block.instructions.windows(2) {
        assert_eq!(pair[0].end(), pair[1].address);
    }
    for instruction in block.instructions.iter() {
        let objdump = &oracle.instructions[&(instruction.address.get() - bias)];
        assert_eq!(*instruction.bytes, *objdump.bytes);
    }

    // Any other instruction start inside the function is reached from its
    // entry; an address inside an instruction is crossed.
    let function = location.image.symbol.expect("crash_segv has a symbol");
    let entry = pc - function.offset;
    let inner = oracle
        .instructions
        .range(entry - bias + 1..pc - bias)
        .find(|(_, instruction)| instruction.bytes.len() > 1)
        .map(|(address, _)| bias + address)
        .expect("a multi-byte instruction precedes the fault");
    let (boundary, leading, block) = into_window(disassemble(&scenario, window(inner, 1, 1)).await);
    assert!(
        matches!(boundary, TargetBoundary::Reached { from, .. } if from.get() == entry),
        "{boundary:?}"
    );
    assert_eq!(leading, None);
    assert_eq!(block.instructions.len(), 2);
    let (boundary, leading, block) =
        into_window(disassemble(&scenario, window(inner + 1, 1, 1)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Crossed {
            from: VirtualAddress::new(entry),
            instruction: VirtualAddress::new(inner),
        }
    );
    assert_eq!(
        leading,
        Some(ContextShortfall::Desynchronized {
            boundary: VirtualAddress::new(inner + 1)
        })
    );
    assert_eq!(block.instructions.len(), 1);
    scenario.shutdown().await;
}

/// A code symbol inside the movabs that decoding from the entry produces is
/// a known start, so decoding resumes there.
async fn assert_marked_data(scenario: &Scenario, marked: u64, context: &str) {
    let disassembly = disassemble(
        scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(marked))),
    )
    .await;
    let DisassemblyView::Function { function, blocks } = &disassembly.view else {
        panic!("not a function");
    };
    assert_eq!(function.name.as_ref(), "disasm_marked_data", "{context}");
    let [block] = &blocks[..] else {
        panic!("{context}: {blocks:?}");
    };
    let starts = block
        .instructions
        .iter()
        .map(|instruction| instruction.address.get() - marked)
        .collect::<Vec<_>>();
    assert_eq!(starts, [0, 2, 4, 9], "{context}");
    // The marker is a code symbol and, where the assembler describes
    // assembly functions, a debug-information function too.
    let [conflict] = &block.conflicts[..] else {
        panic!("{context}: {:?}", block.conflicts);
    };
    let evidence = conflict.evidence;
    assert!(
        matches!(
            evidence,
            BoundaryEvidence::FunctionRange | BoundaryEvidence::CodeSymbol
        ),
        "{context}: {conflict:?}"
    );
    assert_eq!(
        *conflict,
        BoundaryConflict {
            boundary: VirtualAddress::new(marked + 4),
            evidence,
            instruction: VirtualAddress::new(marked + 2),
        },
        "{context}"
    );
    assert_eq!(block.completion, BlockCompletion::Complete);

    // The marker proves its own address, but nothing before it.
    let (boundary, leading, _) = into_window(disassemble(scenario, window(marked + 4, 2, 1)).await);
    assert_eq!(boundary, TargetBoundary::Known(evidence));
    assert!(matches!(
        leading,
        Some(ContextShortfall::Desynchronized { boundary }) if boundary.get() == marked + 4
    ));
    let (boundary, leading, block) =
        into_window(disassemble(scenario, window(marked + 9, 1, 1)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Reached {
            from: VirtualAddress::new(marked + 4),
            evidence,
        }
    );
    assert_eq!(leading, None);
    assert_eq!(block.instructions[0].address.get(), marked + 4);
}

/// Without a marker, the movabs runs past the declared end, and asking for
/// the real code after the data says that the address lies inside an
/// instruction and proves nothing before it.
async fn assert_hidden_data(scenario: &Scenario, hidden: u64, context: &str) {
    let disassembly = disassemble(
        scenario,
        query(DisassemblyRange::Function(VirtualAddress::new(hidden))),
    )
    .await;
    let DisassemblyView::Function { blocks, .. } = &disassembly.view else {
        panic!("not a function");
    };
    let block = &blocks[0];
    assert_eq!(block.instructions.len(), 2, "{context}: {block:#?}");
    assert_eq!(block.instructions[1].bytes.len(), 10);
    assert_eq!(
        *block.conflicts,
        [BoundaryConflict {
            boundary: VirtualAddress::new(hidden + 10),
            evidence: BoundaryEvidence::RangeEnd,
            instruction: VirtualAddress::new(hidden + 2),
        }],
        "{context}"
    );

    let (boundary, leading, block) =
        into_window(disassemble(scenario, window(hidden + 4, 4, 2)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Crossed {
            from: VirtualAddress::new(hidden),
            instruction: VirtualAddress::new(hidden + 2),
        },
        "{context}"
    );
    assert_eq!(
        leading,
        Some(ContextShortfall::Desynchronized {
            boundary: VirtualAddress::new(hidden + 4)
        })
    );
    let texts = block
        .instructions
        .iter()
        .map(|instruction| decoded(instruction).text())
        .collect::<Vec<_>>();
    assert_eq!(texts, ["mov $1, %eax", "ret"], "{context}");
}

#[tokio::test]
async fn data_inside_code_is_reported_instead_of_hidden() {
    for fixture in ["disassembly-gcc-o0", "disassembly-clang-o2-nopie"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main").await;
        scenario.run_to_stop().await;
        let modules = Modules::load(&scenario).await;
        let hidden = modules.symbol(fixture, "disasm_hidden_data");
        assert_marked_data(
            &scenario,
            modules.symbol(fixture, "disasm_marked_data"),
            fixture,
        )
        .await;
        assert_hidden_data(&scenario, hidden, fixture).await;

        // Stopped at the hidden code, the program counter proves it, and the
        // trap installed there is never shown.
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(hidden + 4)))
            .await;
        let stop = scenario.resume_to_stop().await;
        assert!(matches!(stop, StopReason::Breakpoint { address } if address.get() == hidden + 4));
        let (boundary, leading, block) =
            into_window(disassemble(&scenario, window(hidden + 4, 4, 2)).await);
        assert_eq!(
            boundary,
            TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
        );
        assert!(matches!(
            leading,
            Some(ContextShortfall::Desynchronized { .. })
        ));
        assert_eq!(*block.instructions[0].bytes, [0xb8, 1, 0, 0, 0]);
        scenario.shutdown().await;
    }
}

/// Disassembles a function in both syntaxes and checks that only the text
/// differs, returning the Intel rendering.
async fn assert_syntaxes_agree(
    scenario: &Scenario,
    function: VirtualAddress,
) -> Vec<DisassembledInstruction> {
    let mut renderings = Vec::new();
    for syntax in [AssemblySyntax::Intel, AssemblySyntax::Att] {
        let disassembly = scenario
            .operation(
                "disassemble",
                scenario.handle().disassemble(DisassemblyQuery {
                    range: DisassemblyRange::Function(function),
                    syntax,
                }),
            )
            .await;
        assert_eq!(disassembly.syntax, syntax);
        let DisassemblyView::Function { blocks, .. } = disassembly.view else {
            panic!("not a function");
        };
        renderings.push(blocks[0].instructions.to_vec());
    }
    let att = renderings.pop().expect("AT&T");
    let intel = renderings.pop().expect("Intel");
    assert_eq!(intel.len(), att.len());
    for (intel, att) in intel.iter().zip(att.iter()) {
        assert_eq!((intel.address, &intel.bytes), (att.address, &att.bytes));
        let (intel, att) = (decoded(intel), decoded(att));
        assert_eq!(intel.flow, att.flow);
        assert_eq!(intel.references, att.references);
    }
    assert!(
        att.iter()
            .any(|instruction| decoded(instruction).text().contains('%'))
    );
    assert!(
        !intel
            .iter()
            .any(|instruction| decoded(instruction).text().contains('%'))
    );
    intel
}

/// Returns each direct call's target with the section and symbol naming it.
fn call_targets(
    instructions: &[DisassembledInstruction],
) -> Vec<(u64, Option<String>, Option<String>)> {
    instructions
        .iter()
        .map(decoded)
        .filter(|instruction| instruction.flow == ControlFlow::Call)
        .map(|instruction| {
            let reference = &instruction.references[0];
            assert_eq!(reference.kind, InstructionReferenceKind::BranchTarget);
            let module = reference.description.module.as_ref().expect("in a module");
            (
                reference.address.get(),
                module
                    .image
                    .section
                    .as_ref()
                    .map(|section| section.name.to_string()),
                module
                    .image
                    .symbol
                    .as_ref()
                    .map(|symbol| symbol.name.to_string()),
            )
        })
        .collect()
}

#[tokio::test]
async fn instructions_name_their_targets_and_render_in_both_syntaxes() {
    for fixture in ["disassembly-gcc-o0", "disassembly-clang-o2-nopie"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main").await;
        scenario.run_to_stop().await;
        let modules = Modules::load(&scenario).await;
        let helper = modules.symbol(fixture, "disasm_helper");
        let main = assert_syntaxes_agree(
            &scenario,
            VirtualAddress::new(modules.symbol(fixture, "main")),
        )
        .await;

        // Calls name their targets: a function by its symbol, the C
        // library's entry through the linkage table by its section alone.
        let calls = call_targets(&main);
        assert!(
            calls.contains(&(
                helper,
                Some(".text".to_owned()),
                Some("disasm_helper".to_owned())
            )),
            "{fixture}: {calls:?}"
        );
        assert!(
            calls.iter().any(|(_, section, symbol)| {
                section
                    .as_deref()
                    .is_some_and(|name| name.starts_with(".plt"))
                    && symbol.is_none()
            }),
            "{fixture}: {calls:?}"
        );

        // The helper's operand names the global it reads.
        let disassembly = disassemble(
            &scenario,
            query(DisassemblyRange::Function(VirtualAddress::new(helper))),
        )
        .await;
        let DisassemblyView::Function { blocks, .. } = disassembly.view else {
            panic!("not a function");
        };
        let operands = blocks[0]
            .instructions
            .iter()
            .flat_map(|instruction| decoded(instruction).references.iter())
            .filter(|reference| reference.kind == InstructionReferenceKind::MemoryOperand)
            .collect::<Vec<_>>();
        let [operand] = operands[..] else {
            panic!("{fixture}: {operands:#?}");
        };
        assert_eq!(
            operand.address.get(),
            modules.symbol(fixture, "disasm_counter")
        );
        let symbol = operand
            .description
            .module
            .as_ref()
            .and_then(|module| module.image.symbol.as_ref())
            .expect("named global");
        assert_eq!(
            (symbol.name.as_ref(), symbol.kind, symbol.offset),
            ("disasm_counter", SymbolKind::Data, 0)
        );

        // Debug information places the program's instructions in its source.
        let module = modules.named(fixture).1;
        let sources = main
            .iter()
            .filter_map(|instruction| instruction.source.as_ref())
            .filter_map(|source| module.source_file(source.file))
            .map(|file| {
                file.path
                    .file_name()
                    .expect("file")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(sources, BTreeSet::from(["main.c".to_owned()]), "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn code_that_no_verified_file_or_dump_holds_is_unreadable() {
    // The library's file was deleted after the dump, which saved no code.
    let core = "core-missing-library/crash.core";
    let scenario = Scenario::open_core(core, &CoreDumpOptions::new(Scenario::fixture(core)));
    let info = scenario
        .handle()
        .core_dump()
        .expect("core")
        .as_ref()
        .clone();
    let missing = info
        .modules
        .iter()
        .find(|module| module.recorded_path.ends_with("libcrash.so"))
        .expect("libcrash.so is recorded");
    assert_eq!(missing.state, CoreModuleState::Missing);
    let data = fs::read(Scenario::fixture("libcrash.so")).expect("read libcrash.so");
    let text = object::File::parse(data.as_slice())
        .expect("parse libcrash.so")
        .section_by_name(".text")
        .expect("text")
        .address();
    let address = missing.start.get() + text;
    // Whatever module precedes the library, nothing proven reaches its code.
    let (boundary, leading, block) =
        into_window(disassemble(&scenario, window(address, 2, 4)).await);
    assert!(
        matches!(
            boundary,
            TargetBoundary::Unverified(
                ContextShortfall::NoKnownBoundary | ContextShortfall::Unreadable { .. }
            )
        ),
        "{boundary:?}"
    );
    assert!(leading.is_some());
    assert!(block.instructions.is_empty());
    assert!(matches!(
        block.completion,
        BlockCompletion::Unreadable { address: stop, .. } if stop.get() == address
    ));
    assert!(block.instructions.is_empty());
    scenario.shutdown().await;

    // A dump that saved no memory cannot verify its executable, which then
    // lends its metadata but never its bytes.
    let options = CoreDumpOptions {
        allow_module_mismatch: true,
        ..CoreDumpOptions::new(Scenario::fixture("crash-gcc-o0-memoryless.core"))
    };
    let unverified = Scenario::open_core("unverified", &options);
    let location = unverified
        .operation("location", unverified.handle().current_location())
        .await;
    let pc = location.address;
    let (boundary, leading, block) =
        into_window(disassemble(&unverified, window(pc.get(), 2, 2)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Known(BoundaryEvidence::ProgramCounter)
    );
    let entry = pc.get() - location.image.symbol.expect("crash_segv").offset;
    assert_eq!(
        leading,
        Some(ContextShortfall::Unreadable {
            address: VirtualAddress::new(entry)
        })
    );
    assert!(block.instructions.is_empty());
    assert!(matches!(
        block.completion,
        BlockCompletion::Unreadable { address, .. } if address == pc
    ));
    // The function's ranges come from the metadata; its bytes do not.
    let function = disassemble(&unverified, query(DisassemblyRange::Function(pc))).await;
    let DisassemblyView::Function { function, blocks } = function.view else {
        panic!("not a function");
    };
    assert_eq!(function.name.as_ref(), "crash_segv");
    assert!(blocks[0].instructions.is_empty());
    assert!(matches!(
        blocks[0].completion,
        BlockCompletion::Unreadable { address, .. } if address.get() == entry
    ));
    unverified.shutdown().await;
}

#[tokio::test]
async fn requests_are_validated() {
    let mut scenario = Scenario::launch("crash-gcc-o0");
    assert!(matches!(
        scenario.handle().disassemble(window(0x1000, 0, 1)).await,
        Err(Error::NotRunning)
    ));
    scenario.run_to_stop().await;
    for (before, after) in [
        (0, 0),
        (MAX_WINDOW_BEFORE + 1, 1),
        (0, MAX_WINDOW_AFTER + 1),
    ] {
        assert!(matches!(
            scenario.handle().disassemble(window(0x1000, before, after)).await,
            Err(Error::InvalidDisassemblyWindow { before: b, after: a }) if (b, a) == (before, after)
        ));
    }
    assert!(matches!(
        scenario
            .handle()
            .disassemble(query(DisassemblyRange::Function(VirtualAddress::new(8))))
            .await,
        Err(Error::NoFunctionContainsAddress(address)) if address.get() == 8
    ));
    // Outside every module, nothing is readable or known.
    let (boundary, _, block) = into_window(disassemble(&scenario, window(8, 1, 1)).await);
    assert_eq!(
        boundary,
        TargetBoundary::Unverified(ContextShortfall::NoKnownBoundary)
    );
    assert!(block.instructions.is_empty());
    scenario.shutdown().await;
}
