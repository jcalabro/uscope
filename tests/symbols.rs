mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;

use support::Scenario;
use uscope::{
    Backtrace, CoreDumpOptions, EmbeddedSymbolTable, Error, ImageAddress, LoadedModuleSnapshot,
    ModuleImage, StackFrame, StopReason, SymbolBinding, SymbolExtentProvenance, SymbolInfo,
    SymbolKind, SymbolLocation, UnwindTermination,
};

/// Each executable of the ELF symbol fixture and the library it loads. The
/// gcc, clang, and non-PIE builds load libraries with full symbol tables.
const VARIANTS: [(&str, &str); 5] = [
    ("elf-symbols-gcc-o0", "libelf-symbols-gcc.so"),
    ("elf-symbols-clang-o2", "libelf-symbols-clang.so"),
    ("elf-symbols-gcc-nopie", "libelf-symbols-gcc.so"),
    ("elf-symbols-stripped", "libelf-symbols-stripped.so"),
    ("elf-symbols-minidebug", "libelf-symbols-minidebug.so"),
];

/// What one frame of the fixture's fault backtrace must show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    /// Library code named by a symbol with a declared size.
    Library(&'static str),
    /// Library code named by an unsized symbol.
    UnsizedLibrary(&'static str),
    /// Library code that no symbol names.
    Unnamed,
    /// Main-image code that debug information describes.
    Debug(&'static str),
    /// C-library code that some symbol names.
    LibC,
    /// C-library code named by a known public symbol.
    LibCSymbol(&'static str),
    /// Main-image code that only its symbol names.
    MainSymbol(&'static str),
}

/// The fault backtrace from the innermost frame outward. Stripping removes
/// the static helper's only symbol.
fn expected_chain(library: &str) -> Vec<Expected> {
    use Expected::{Debug, LibC, LibCSymbol, Library, MainSymbol, Unnamed, UnsizedLibrary};
    let helper = if library == "libelf-symbols-stripped.so" {
        Unnamed
    } else {
        Library("lib_static_helper")
    };
    vec![
        Library("lib_fault"),
        Debug("chain_fault"),
        UnsizedLibrary("asm_unsized"),
        Debug("chain_unsized"),
        Library("asm_sized"),
        Debug("chain_sized"),
        Library("asm_resolver_impl"),
        Debug("chain_resolver"),
        Library("asm_alias_global"),
        Debug("chain_alias"),
        Debug("chain_nested_site"),
        Library("asm_nested_outer"),
        Debug("chain_nested"),
        Library("asm_noreturn_caller"),
        Debug("chain_noreturn"),
        Unnamed,
        Library("asm_gap_entry"),
        Debug("chain_gap"),
        helper,
        Library("lib_exported_entry"),
        Debug("main"),
        LibC,
        LibCSymbol("__libc_start_main"),
        MainSymbol("_start"),
    ]
}

/// The modules of one stopped process, with their file names and images.
struct Modules {
    snapshot: LoadedModuleSnapshot,
    images: BTreeMap<uscope::ModuleId, Arc<ModuleImage>>,
}

impl Modules {
    async fn load(scenario: &Scenario) -> Self {
        let snapshot = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        let mut images = BTreeMap::new();
        for record in snapshot.modules.iter() {
            images.insert(
                record.module.id,
                scenario
                    .operation(
                        "module image",
                        scenario.handle().loaded_module_image(record.module.id),
                    )
                    .await,
            );
        }
        Self { snapshot, images }
    }

    fn file_name(&self, module: uscope::ModuleId) -> String {
        let record = self
            .snapshot
            .modules
            .iter()
            .find(|record| record.module.id == module)
            .unwrap_or_else(|| panic!("unknown module {module:?}"));
        record
            .path
            .file_name()
            .expect("module path names a file")
            .to_string_lossy()
            .into_owned()
    }

    fn bias(&self, module: uscope::ModuleId) -> u64 {
        self.snapshot
            .modules
            .iter()
            .find(|record| record.module.id == module)
            .expect("known module")
            .module
            .load_bias
    }

    fn named(&self, file_name: &str) -> (uscope::ModuleId, &ModuleImage) {
        let record = self
            .snapshot
            .modules
            .iter()
            .find(|record| {
                record
                    .path
                    .file_name()
                    .is_some_and(|name| name == file_name)
            })
            .unwrap_or_else(|| panic!("no loaded module named {file_name}"));
        (record.module.id, &self.images[&record.module.id])
    }
}

fn symbol_named<'a>(image: &'a ModuleImage, name: &str) -> &'a SymbolInfo {
    let matches = image
        .symbols()
        .iter()
        .filter(|symbol| symbol.name.as_ref() == name)
        .collect::<Vec<_>>();
    let [symbol] = matches.as_slice() else {
        panic!("expected one symbol named {name}, found {matches:#?}");
    };
    symbol
}

fn frame_symbol<'a>(frame: &'a StackFrame, context: &str) -> &'a SymbolLocation {
    frame.symbol.as_ref().unwrap_or_else(|| {
        panic!(
            "{context}: frame #{} has no symbol: {frame:#?}",
            frame.level
        )
    })
}

/// Checks every frame's symbol against its module's catalog: the symbol
/// exists, its offset is measured to the frame's own instruction, and its
/// extent contains the address the frame was looked up by.
fn assert_frame_symbols_are_consistent(trace: &Backtrace, modules: &Modules, context: &str) {
    for frame in trace.frames.iter() {
        let Some(symbol) = &frame.symbol else {
            continue;
        };
        let module = frame
            .module
            .unwrap_or_else(|| panic!("{context}: symbolized frame without a module"));
        let image = &modules.images[&module];
        let info = image
            .symbol(symbol.symbol)
            .unwrap_or_else(|| panic!("{context}: unknown symbol {symbol:?}"));
        assert_eq!(info.name, symbol.name, "{context}");
        let image_instruction = frame.instruction.get() - modules.bias(module);
        assert_eq!(
            image_instruction - info.address.get(),
            symbol.offset,
            "{context}: frame #{}",
            frame.level
        );
        let extent = info.extent.expect("a frame symbol names code");
        assert_eq!(extent.provenance, symbol.provenance, "{context}");
        let lookup = if frame.level == 0 {
            image_instruction
        } else {
            image_instruction - 1
        };
        assert!(
            extent.range.contains(ImageAddress::new(lookup)),
            "{context}: frame #{} lookup {lookup:#x} is outside {extent:?}",
            frame.level
        );
    }
}

/// Checks the fault backtrace frame by frame, then the layout facts that make
/// each case meaningful.
fn assert_fault_chain(trace: &Backtrace, modules: &Modules, executable: &str, library: &str) {
    let context = format!("{executable} with {library}");
    let expected = expected_chain(library);
    assert_eq!(
        trace.termination,
        UnwindTermination::Complete,
        "{context}: {trace:#?}"
    );
    assert_eq!(trace.frames.len(), expected.len(), "{context}: {trace:#?}");
    assert_frame_symbols_are_consistent(trace, modules, &context);

    for (frame, expected) in trace.frames.iter().zip(&expected) {
        let module = modules.file_name(
            frame
                .module
                .unwrap_or_else(|| panic!("{context}: frame #{} has no module", frame.level)),
        );
        let frame_context = format!("{context}: frame #{} {expected:?}", frame.level);
        match *expected {
            Expected::Library(name) | Expected::UnsizedLibrary(name) => {
                assert_eq!(module, library, "{frame_context}");
                let symbol = frame_symbol(frame, &frame_context);
                assert_eq!(symbol.name.as_ref(), name, "{frame_context}");
                assert_eq!(
                    symbol.provenance,
                    if matches!(expected, Expected::Library(_)) {
                        SymbolExtentProvenance::Declared
                    } else {
                        SymbolExtentProvenance::Inferred
                    },
                    "{frame_context}"
                );
                // Symbols never stand in for debug information.
                assert!(frame.function.is_none(), "{frame_context}");
                assert!(frame.source.is_none(), "{frame_context}");
                assert!(frame.code_instance.is_none(), "{frame_context}");
            }
            Expected::Unnamed => {
                assert_eq!(module, library, "{frame_context}");
                assert!(frame.symbol.is_none(), "{frame_context}: {frame:#?}");
                assert!(frame.function.is_none(), "{frame_context}");
            }
            Expected::Debug(name) => {
                assert_eq!(module, executable, "{frame_context}");
                let function = frame.function.as_ref().expect("debug-info function");
                assert_eq!(function.name.as_ref(), name, "{frame_context}");
                assert!(frame.source.is_some(), "{frame_context}");
                // Debug-info frames also carry their linkage symbol.
                let symbol = frame_symbol(frame, &frame_context);
                assert_eq!(symbol.name.as_ref(), name, "{frame_context}");
                assert_eq!(symbol.provenance, SymbolExtentProvenance::Declared);
            }
            Expected::LibC | Expected::LibCSymbol(_) => {
                assert!(module.starts_with("libc.so"), "{frame_context}: {module}");
                let symbol = frame_symbol(frame, &frame_context);
                if let Expected::LibCSymbol(name) = expected {
                    assert_eq!(symbol.name.as_ref(), *name, "{frame_context}");
                }
            }
            Expected::MainSymbol(name) => {
                assert_eq!(module, executable, "{frame_context}");
                assert!(frame.function.is_none(), "{frame_context}");
                assert_eq!(
                    frame_symbol(frame, &frame_context).name.as_ref(),
                    name,
                    "{frame_context}"
                );
            }
        }
    }

    assert_library_layout(trace, modules, library, &expected, &context);
}

/// Checks the layout facts that make each library case of the fault chain
/// meaningful.
fn assert_library_layout(
    trace: &Backtrace,
    modules: &Modules,
    library: &str,
    expected: &[Expected],
    context: &str,
) {
    let (_, image) = modules.named(library);
    let position = |name: &str| {
        expected
            .iter()
            .position(|frame| {
                matches!(frame, Expected::Library(n) | Expected::UnsizedLibrary(n) if *n == name)
            })
            .expect("expected frame")
    };

    // The callback call ends asm_noreturn_caller, so the return address is
    // asm_after_noreturn's first byte; the frame still belongs to the caller.
    let noreturn = &trace.frames[position("asm_noreturn_caller")];
    let context = context.to_owned();
    let caller = symbol_named(image, "asm_noreturn_caller");
    let after = symbol_named(image, "asm_after_noreturn");
    let caller_extent = caller.extent.expect("sized caller").range;
    assert_eq!(caller_extent.end, after.address, "{context}");
    assert_eq!(
        frame_symbol(noreturn, &context).offset,
        caller_extent.end.get() - caller_extent.start.get(),
        "{context}"
    );

    // The chain continues from the call after the nested symbol ends.
    let outer = &trace.frames[position("asm_nested_outer")];
    let inner = symbol_named(image, "asm_nested_inner")
        .extent
        .expect("sized inner")
        .range;
    let outer_offset = frame_symbol(outer, &context).offset;
    assert!(
        outer_offset > inner.end.get() - symbol_named(image, "asm_nested_outer").address.get(),
        "{context}: {outer_offset:#x}"
    );

    // Every name of the alias group shares one extent, and binding and
    // underscores select the global name.
    let alias = symbol_named(image, "asm_alias_global");
    let mut aliases = image
        .symbols()
        .iter()
        .filter(|symbol| symbol.address == alias.address)
        .map(|symbol| {
            assert_eq!(symbol.extent, alias.extent, "{context}");
            (symbol.name.as_ref(), symbol.binding)
        })
        .collect::<Vec<_>>();
    aliases.sort_unstable();
    let mut expected_aliases = vec![
        ("__asm_alias_global", SymbolBinding::Global),
        ("asm_alias_a_weak", SymbolBinding::Weak),
        ("asm_alias_global", SymbolBinding::Global),
    ];
    if library != "libelf-symbols-stripped.so" {
        expected_aliases.push(("asm_alias_0_local", SymbolBinding::Local));
    }
    expected_aliases.sort_unstable();
    assert_eq!(aliases, expected_aliases, "{context}");

    // The resolver's ordinary name wins over the indirect-function symbol
    // that shares its extent.
    let resolver = symbol_named(image, "asm_resolver_impl");
    let indirect = symbol_named(image, "asm_indirect");
    assert_eq!(indirect.kind, SymbolKind::IndirectFunction, "{context}");
    assert_eq!(resolver.kind, SymbolKind::Function, "{context}");
    assert_eq!(indirect.extent, resolver.extent, "{context}");

    // An unsized symbol with its own call-frame entry ends with that entry.
    let oracle = Oracle::read(&[library]);
    let unsized_symbol = symbol_named(image, "asm_unsized");
    let unsized_extent = unsized_symbol.extent.expect("code");
    assert_eq!(
        unsized_extent.range.end.get(),
        oracle.unwind_end_at(unsized_symbol.address.get()),
        "{context}"
    );
    assert_eq!(unsized_extent.provenance, SymbolExtentProvenance::Inferred);

    // Without one, it ends where the next function begins. Once stripping
    // removes that function's symbol, only its call-frame entry bounds it.
    let leaf_extent = symbol_named(image, "asm_unsized_leaf")
        .extent
        .expect("code");
    let next_function = if library == "libelf-symbols-stripped.so" {
        assert!(
            !image
                .symbols()
                .iter()
                .any(|symbol| symbol.name.as_ref() == "asm_local_after_unsized"),
            "{context}"
        );
        Oracle::read(&["libelf-symbols-stripped.so.full"]).address_of("asm_local_after_unsized")
    } else {
        symbol_named(image, "asm_local_after_unsized").address.get()
    };
    assert_eq!(leaf_extent.range.end.get(), next_function, "{context}");
    assert_eq!(leaf_extent.provenance, SymbolExtentProvenance::Inferred);

    let sources = image.symbol_sources();
    assert!(sources.dynamic_table, "{context}");
    match library {
        "libelf-symbols-stripped.so" => {
            assert!(!sources.static_table, "{context}");
            assert_eq!(sources.embedded_table, EmbeddedSymbolTable::Absent);
        }
        "libelf-symbols-minidebug.so" => {
            assert!(!sources.static_table, "{context}");
            assert_eq!(sources.embedded_table, EmbeddedSymbolTable::Loaded);
        }
        _ => {
            assert!(sources.static_table, "{context}");
            assert_eq!(sources.embedded_table, EmbeddedSymbolTable::Absent);
        }
    }
}

fn open_core(executable: &str) -> Scenario {
    Scenario::open_core(
        executable,
        &CoreDumpOptions::new(Scenario::fixture(&format!("{executable}.core"))),
    )
}

#[tokio::test]
async fn frames_in_code_without_debug_info_are_named_by_elf_symbols() {
    for (executable, library) in VARIANTS {
        let mut scenario = Scenario::launch(executable);
        let site = scenario.add_breakpoint("chain_nested_site").await;

        // asm_nested_outer calls back before, within, and after its nested
        // symbol. The first return address is the nested symbol's first byte,
        // so only a lookup before the return address names the outer symbol.
        for expected in ["asm_nested_outer", "asm_nested_inner"] {
            let stop = if expected == "asm_nested_outer" {
                scenario.run_to_stop().await
            } else {
                scenario.resume_to_stop().await
            };
            assert!(matches!(stop, StopReason::Breakpoint { .. }), "{stop:?}");
            let trace = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await;
            let modules = Modules::load(&scenario).await;
            assert_frame_symbols_are_consistent(&trace, &modules, executable);
            let caller = frame_symbol(&trace.frames[1], executable);
            assert_eq!(caller.name.as_ref(), expected, "{executable}: {trace:#?}");
            assert_eq!(caller.provenance, SymbolExtentProvenance::Declared);
        }

        scenario.remove_breakpoint(site.id).await;
        let stop = scenario.resume_to_stop().await;
        assert!(
            matches!(&stop, StopReason::Exception(exception) if exception.code == 4),
            "{executable}: {stop:?}"
        );
        let modules = Modules::load(&scenario).await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        assert_fault_chain(&trace, &modules, executable, library);

        // The stop location belongs to the library that faulted, which has no
        // source to show.
        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        let (library_module, _) = modules.named(library);
        assert_eq!(location.module, library_module, "{executable}");
        assert_eq!(location.address, trace.frames[0].instruction);
        assert!(location.image.function.is_none());
        assert!(location.image.source.is_none());
        assert_eq!(
            location
                .image
                .symbol
                .as_ref()
                .map(|symbol| symbol.name.as_ref()),
            Some("lib_fault"),
            "{executable}"
        );
        assert!(matches!(
            scenario.handle().source_context(3).await,
            Err(Error::SourceLocationUnavailable)
        ));
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn core_dumps_name_frames_without_debug_info_like_live_processes() {
    for (executable, library) in VARIANTS {
        let scenario = open_core(executable);
        let modules = Modules::load(&scenario).await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        assert_fault_chain(&trace, &modules, executable, library);
        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(location.module, modules.named(library).0);
        assert_eq!(location.image.symbol, trace.frames[0].symbol);
        scenario.shutdown().await;
    }
}

/// One frame of gdb's backtrace of a core.
#[derive(Debug)]
struct GdbFrame {
    address: Option<u64>,
    name: String,
    has_source: bool,
}

fn gdb_backtrace(executable: &str) -> Vec<GdbFrame> {
    let path = Scenario::fixture(&format!("{executable}.core.gdb-backtrace"));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.lines()
        .map(|line| {
            // "#N  0xADDRESS in NAME (...) ..." or "#N  NAME (...) at FILE:LINE"
            let rest = line.split_once(' ').expect("frame number").1.trim_start();
            let (address, rest) = match rest.split_once(" in ") {
                Some((address, rest)) if address.starts_with("0x") => (
                    Some(u64::from_str_radix(&address[2..], 16).expect("frame address")),
                    rest,
                ),
                _ => (None, rest),
            };
            GdbFrame {
                address,
                name: rest.split(" (").next().expect("frame name").to_owned(),
                has_source: line.contains(") at "),
            }
        })
        .collect()
}

/// gdb's backtrace of the same core is an independent oracle for both the
/// unwound frames and their names. Names may differ only by choosing another
/// symbol at the same address.
#[tokio::test]
async fn core_backtraces_agree_with_gdb() {
    for (executable, _) in VARIANTS {
        let scenario = open_core(executable);
        let modules = Modules::load(&scenario).await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let gdb = gdb_backtrace(executable);
        assert_eq!(trace.frames.len(), gdb.len(), "{executable}: {gdb:#?}");

        for (frame, gdb) in trace.frames.iter().zip(&gdb) {
            let context = format!("{executable}: frame #{} {gdb:?}", frame.level);
            if let Some(address) = gdb.address {
                assert_eq!(frame.instruction.get(), address, "{context}");
            }
            if gdb.has_source {
                let function = frame.function.as_ref().expect("debug-info frame");
                assert_eq!(function.name.as_ref(), gdb.name, "{context}");
                continue;
            }
            assert!(frame.function.is_none(), "{context}");
            if gdb.name == "??" {
                assert!(frame.symbol.is_none(), "{context}: {frame:#?}");
                continue;
            }
            let symbol = frame_symbol(frame, &context);
            let image = &modules.images[&frame.module.expect("symbolized module")];
            let chosen = image.symbol(symbol.symbol).expect("catalog symbol");
            assert!(
                image
                    .symbols()
                    .iter()
                    .any(|alias| alias.name.as_ref() == gdb.name
                        && alias.address == chosen.address),
                "{context}: gdb's {} is not an alias of {}",
                gdb.name,
                chosen.name
            );
        }
        scenario.shutdown().await;
    }
}

/// readelf's reading of one ELF file's sections and symbol tables.
struct Oracle {
    files: Vec<OracleFile>,
}

struct OracleFile {
    executable_sections: BTreeMap<u32, (u64, u64)>,
    symbols: Vec<OracleSymbol>,
    /// The code range of every call-frame entry.
    unwind: Vec<(u64, u64)>,
}

#[derive(Debug, Clone)]
struct OracleSymbol {
    name: String,
    address: u64,
    size: u64,
    kind: Option<SymbolKind>,
    binding: Option<SymbolBinding>,
    /// The defining section, or `None` for undefined, absolute, and common.
    section: Option<u32>,
    dynamic: bool,
}

impl Oracle {
    /// Reads the oracles of one module. Several files describe a module with
    /// an embedded symbol table.
    fn read(names: &[&str]) -> Self {
        Self {
            files: names.iter().map(|name| OracleFile::read(name)).collect(),
        }
    }

    fn symbols(&self) -> impl Iterator<Item = (&OracleFile, &OracleSymbol)> {
        self.files
            .iter()
            .flat_map(|file| file.symbols.iter().map(move |symbol| (file, symbol)))
    }

    fn unwind_ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.files
            .iter()
            .flat_map(|file| file.unwind.iter().copied())
    }

    fn unwind_end_at(&self, address: u64) -> u64 {
        self.unwind_ranges()
            .filter(|(start, _)| *start == address)
            .map(|(_, end)| end)
            .min()
            .unwrap_or_else(|| panic!("no call-frame entry begins at {address:#x}"))
    }

    fn address_of(&self, name: &str) -> u64 {
        let addresses = self
            .symbols()
            .filter(|(_, symbol)| symbol.name == name)
            .map(|(_, symbol)| symbol.address)
            .collect::<BTreeSet<_>>();
        let [address] = addresses.into_iter().collect::<Vec<_>>()[..] else {
            panic!("{name} does not have exactly one address");
        };
        address
    }
}

impl OracleFile {
    fn read(name: &str) -> Self {
        let path = Scenario::fixture(&format!("symbol-oracles/{name}.readelf"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let mut executable_sections = BTreeMap::new();
        let mut symbols = Vec::new();
        let mut unwind = Vec::new();
        let mut dynamic = false;

        for line in text.lines() {
            let trimmed = line.trim_start();
            // "OFFSET LENGTH CIE_POINTER FDE cie=OFFSET pc=START..END"
            if line.contains(" FDE ")
                && let Some(range) = line.split("pc=").nth(1)
                && let Some((start, end)) = range.split_once("..")
            {
                unwind.push((
                    u64::from_str_radix(start, 16).expect("frame start"),
                    u64::from_str_radix(end.trim(), 16).expect("frame end"),
                ));
                continue;
            }
            if let Some(table) = trimmed.strip_prefix("Symbol table '") {
                dynamic = table.starts_with(".dynsym'");
                continue;
            }
            if let Some(section) = trimmed.strip_prefix('[')
                && let Some((index, rest)) = section.split_once(']')
                && let Ok(index) = index.trim().parse::<u32>()
            {
                // Name Type Address Off Size ES [Flg] Lk Inf Al
                let fields = rest.split_whitespace().collect::<Vec<_>>();
                if fields.len() == 10 && fields[6].contains('A') && fields[6].contains('X') {
                    let start = u64::from_str_radix(fields[2], 16).expect("section address");
                    let size = u64::from_str_radix(fields[4], 16).expect("section size");
                    executable_sections.insert(index, (start, start + size));
                }
                continue;
            }
            // Num: Value Size Type Bind Vis Ndx Name
            let fields = trimmed.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 8
                || !fields[0].ends_with(':')
                || fields[0].trim_end_matches(':').parse::<u32>().is_err()
            {
                continue;
            }
            let size = fields[2].strip_prefix("0x").map_or_else(
                || fields[2].parse().expect("decimal symbol size"),
                |hex| u64::from_str_radix(hex, 16).expect("hex symbol size"),
            );
            symbols.push(OracleSymbol {
                // readelf appends versions to dynamic symbol names. A static
                // table may itself hold versioned names, which are kept.
                name: if dynamic {
                    fields[7].split('@').next().expect("name").to_owned()
                } else {
                    fields[7].to_owned()
                },
                address: u64::from_str_radix(fields[1], 16).expect("symbol value"),
                size,
                kind: match fields[3] {
                    "FUNC" => Some(SymbolKind::Function),
                    "IFUNC" => Some(SymbolKind::IndirectFunction),
                    "OBJECT" => Some(SymbolKind::Data),
                    "NOTYPE" => Some(SymbolKind::Unknown),
                    _ => None,
                },
                binding: match fields[4] {
                    "GLOBAL" | "UNIQUE" => Some(SymbolBinding::Global),
                    "WEAK" => Some(SymbolBinding::Weak),
                    "LOCAL" => Some(SymbolBinding::Local),
                    _ => None,
                },
                section: fields[6].parse().ok(),
                dynamic,
            });
        }
        Self {
            executable_sections,
            symbols,
            unwind,
        }
    }
}

/// Compares one module image's symbols with readelf's reading of the same
/// file: the catalog holds exactly the symbols the policy admits, with the
/// kind, binding, and export status the tables record.
fn assert_catalog_matches_oracle(image: &ModuleImage, oracle: &Oracle, context: &str) {
    let mut admitted = BTreeMap::<(String, u64), Vec<&OracleSymbol>>::new();
    for (_, symbol) in oracle.symbols() {
        if symbol.section.is_some()
            && symbol.kind.is_some()
            && symbol.binding.is_some()
            && !symbol.name.is_empty()
        {
            admitted
                .entry((symbol.name.clone(), symbol.address))
                .or_default()
                .push(symbol);
        }
    }
    let catalog = image
        .symbols()
        .iter()
        .map(|symbol| ((symbol.name.to_string(), symbol.address.get()), symbol))
        .collect::<BTreeMap<_, _>>();
    let invented = catalog
        .keys()
        .filter(|key| !admitted.contains_key(*key))
        .collect::<Vec<_>>();
    let missing = admitted
        .keys()
        .filter(|key| !catalog.contains_key(*key))
        .collect::<Vec<_>>();
    assert!(
        invented.is_empty() && missing.is_empty(),
        "{context}: invented {invented:?}, missing {missing:?}"
    );
    for (key, entries) in &admitted {
        let symbol = catalog[key];
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == Some(symbol.kind)
                    && entry.binding == Some(symbol.binding)),
            "{context}: {symbol:?} vs {entries:?}"
        );
        assert_eq!(
            symbol.exported,
            entries.iter().any(|entry| entry.dynamic),
            "{context}: {symbol:?}"
        );
    }
    assert_code_extents_match_oracle(image, oracle, &catalog, context);
}

/// Checks every code symbol's extent against the sizes, sections, and
/// call-frame entries readelf reports, and probes lookups at both ends of
/// every sized function.
fn assert_code_extents_match_oracle(
    image: &ModuleImage,
    oracle: &Oracle,
    catalog: &BTreeMap<(String, u64), &SymbolInfo>,
    context: &str,
) {
    // The start of every code symbol in each executable section bounds the
    // inferred extents of unsized symbols.
    let mut code_starts = BTreeSet::new();
    let mut code_symbols = Vec::new();
    for (file, symbol) in oracle.symbols() {
        if !matches!(
            symbol.kind,
            Some(SymbolKind::Function | SymbolKind::IndirectFunction)
        ) {
            continue;
        }
        let Some(&(start, end)) = symbol
            .section
            .and_then(|section| file.executable_sections.get(&section))
        else {
            continue;
        };
        code_starts.insert(symbol.address);
        code_symbols.push((symbol, start, end));
    }

    let mut code_by_key = BTreeMap::new();
    for (symbol, start, end) in code_symbols {
        code_by_key
            .entry((symbol.name.clone(), symbol.address))
            .or_insert((symbol, start, end));
    }

    let unwind_starts = oracle
        .unwind_ranges()
        .filter(|(start, end)| start < end)
        .map(|(start, _)| start)
        .collect::<BTreeSet<_>>();
    let mut checked = 0_usize;
    let mut inferred = 0_usize;
    for (key, symbol) in catalog {
        let Some(&(oracle_symbol, _, section_end)) = code_by_key.get(key) else {
            assert!(symbol.extent.is_none(), "{context}: {symbol:?}");
            continue;
        };
        let extent = symbol
            .extent
            .unwrap_or_else(|| panic!("{context}: code symbol without extent {symbol:?}"));
        assert_eq!(extent.range.start, symbol.address, "{context}");
        if oracle_symbol.size == 0 {
            // The first evidence of other code: the next code symbol, the next
            // call-frame entry, the end of an entry beginning at the symbol,
            // or the section end.
            let address = symbol.address.get();
            let end = code_starts
                .range(address + 1..)
                .next()
                .copied()
                .into_iter()
                .chain(unwind_starts.range(address + 1..).next().copied())
                .chain(
                    oracle
                        .unwind_ranges()
                        .filter(|(start, _)| *start == address)
                        .map(|(_, end)| end),
                )
                .fold(section_end, u64::min);
            assert_eq!(extent.provenance, SymbolExtentProvenance::Inferred);
            assert_eq!(extent.range.end.get(), end, "{context}: {symbol:?}");
            inferred += 1;
            continue;
        }
        assert_eq!(extent.provenance, SymbolExtentProvenance::Declared);
        assert_eq!(
            extent.range.end.get() - extent.range.start.get(),
            oracle_symbol.size,
            "{context}: {symbol:?}"
        );

        for address in [
            extent.range.start,
            ImageAddress::new(extent.range.end.get() - 1),
        ] {
            let found = image
                .symbolize(address)
                .unwrap_or_else(|| panic!("{context}: {address:#x} in {symbol:?} is unnamed"));
            let found = image.symbol(found.symbol).expect("catalog symbol");
            let found_extent = found.extent.expect("code").range;
            assert!(
                found_extent.contains(address)
                    && found_extent.start >= extent.range.start
                    && (found_extent.start > extent.range.start
                        || found_extent.end <= extent.range.end),
                "{context}: {address:#x} in {symbol:?} selected {found:?}"
            );
        }
        if let Some(after) = image.symbolize(extent.range.end) {
            let after = image.symbol(after.symbol).expect("catalog symbol");
            assert_ne!(after.extent.map(|extent| extent.range), Some(extent.range));
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "{context}: no sized code symbols were compared"
    );
    assert!(
        inferred > 0 || !context.contains("libelf-symbols"),
        "{context}: no unsized code symbols were compared"
    );
}

#[tokio::test]
async fn symbol_catalogs_match_binutils_for_every_fixture_module() {
    for (executable, library) in VARIANTS {
        let scenario = open_core(executable);
        let modules = Modules::load(&scenario).await;
        let mut compared = BTreeSet::new();
        for record in modules.snapshot.modules.iter() {
            let file_name = record
                .path
                .file_name()
                .expect("module file")
                .to_string_lossy()
                .into_owned();
            let embedded = format!("{file_name}.embedded");
            let oracles = if file_name == "libelf-symbols-minidebug.so" {
                vec![file_name.as_str(), embedded.as_str()]
            } else {
                vec![file_name.as_str()]
            };
            if !Scenario::fixture(&format!("symbol-oracles/{file_name}.readelf")).exists() {
                continue;
            }
            assert_catalog_matches_oracle(
                &modules.images[&record.module.id],
                &Oracle::read(&oracles),
                &format!("{executable}: {file_name}"),
            );
            compared.insert(file_name);
        }
        for required in [executable, library, "libc.so.6", "ld-linux-x86-64.so.2"] {
            assert!(
                compared.contains(required),
                "{executable}: {required} was not compared: {compared:?}"
            );
        }
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn rust_frames_without_debug_info_are_named_by_demangled_symbols() {
    let scenario = open_core("crash-rust-nodebug");
    let modules = Modules::load(&scenario).await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    assert_frame_symbols_are_consistent(&trace, &modules, "rust");
    assert_eq!(trace.termination, UnwindTermination::Complete);
    let names = trace
        .frames
        .iter()
        .map(|frame| {
            assert!(frame.function.is_none(), "{frame:#?}");
            frame.symbol.as_ref().map(|symbol| {
                (
                    modules.file_name(frame.module.expect("module")),
                    symbol
                        .demangled_name()
                        .unwrap_or_else(|| symbol.name.to_string()),
                )
            })
        })
        .collect::<Vec<_>>();
    // Optimization is off, so the volatile write is a call of its own.
    let executable = "crash-rust-nodebug".to_owned();
    assert_eq!(
        names[..3],
        [
            Some((
                executable.clone(),
                "core::ptr::write_volatile::<i32>".to_owned()
            )),
            Some((executable.clone(), "crash::crash_now".to_owned())),
            Some((executable.clone(), "main".to_owned())),
        ],
        "{names:#?}"
    );
    assert!(
        trace.frames[1]
            .symbol
            .as_ref()
            .is_some_and(|symbol| symbol.name.starts_with("_R")),
        "the raw name stays the v0-mangled linkage name"
    );
    assert_eq!(
        names.last().cloned().flatten(),
        Some((executable, "_start".to_owned()))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn go_frames_without_dwarf_are_named_but_not_unwound() {
    let scenario = open_core("crash-go-nodwarf");
    let modules = Modules::load(&scenario).await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    assert_frame_symbols_are_consistent(&trace, &modules, "go");
    // Without DWARF, Go code has no call-frame information to unwind with,
    // and the debugger says so rather than guessing a caller.
    assert!(
        matches!(trace.termination, UnwindTermination::NoUnwindInfo { .. }),
        "{trace:#?}"
    );
    let [frame] = &trace.frames[..] else {
        panic!("{trace:#?}");
    };
    assert!(frame.function.is_none());
    assert_eq!(
        frame.symbol.as_ref().map(|symbol| symbol.name.as_ref()),
        Some("main.crashNow")
    );
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(location.image.symbol, frame.symbol);
    scenario.shutdown().await;
}

#[tokio::test]
async fn thread_local_symbols_have_no_runtime_address() {
    let mut scenario = Scenario::new("tls", Scenario::fixture("globals-tls-gcc"));
    scenario.add_breakpoint("main").await;
    scenario.run_to_stop().await;
    // A thread-local symbol's value is an offset into each thread's block,
    // so relocating it like an address would name unrelated memory.
    assert!(matches!(
        scenario.handle().runtime_address("tls_pointer").await,
        Err(Error::SymbolNotFound(name)) if name == "tls_pointer"
    ));
    let main = scenario
        .operation("main address", scenario.handle().runtime_address("main"))
        .await;
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    let symbol = location.image.symbol.expect("main is a code symbol");
    assert_eq!(symbol.name.as_ref(), "main");
    assert_eq!(main.get() + symbol.offset, location.address.get());
    scenario.shutdown().await;
}

#[tokio::test]
async fn instructions_outside_every_module_have_no_location() {
    let mut live = Scenario::new("null call", Scenario::fixture("null-call"));
    let stop = live.run_to_stop().await;
    assert!(
        matches!(&stop, StopReason::Exception(exception) if exception.code == 11),
        "{stop:?}"
    );
    for scenario in [live, open_core("null-call")] {
        // No module describes address zero, so there is no location to report
        // and nothing to unwind with, rather than a guess from the main image.
        assert!(matches!(
            scenario.handle().current_location().await,
            Err(Error::AddressOutsideModule)
        ));
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let [frame] = &trace.frames[..] else {
            panic!("{trace:#?}");
        };
        assert_eq!(frame.instruction.get(), 0);
        assert!(frame.module.is_none() && frame.symbol.is_none() && frame.function.is_none());
        assert!(
            matches!(trace.termination, UnwindTermination::ModuleNotFound { address } if address.get() == 0),
            "{trace:#?}"
        );
        scenario.shutdown().await;
    }
}
