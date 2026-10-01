//! Disassembly of a stopped snapshot's memory.

use std::collections::BTreeMap;

use nix::unistd::Pid;

use crate::disassembly::{CodeRead, CodeSource, Engine, decoder_for};
use crate::protocol::StopId;
use crate::{
    AddressDescription, AddressRange, BoundaryEvidence, DisassembledFunction, Disassembly,
    DisassemblyQuery, DisassemblyRange, DisassemblyView, Error, FunctionOrigin, MAX_WINDOW_AFTER,
    MAX_WINDOW_BEFORE, Result, SourceLocation, VirtualAddress,
};

use super::frames::{UnwindModule, describe_address, unwind_module_for};
use super::memory::read_logical_memory;
use super::native::InspectionOps;
use super::{
    BreakpointSite, Controller, validate_image_current, validate_public_stop,
    validate_stopped_thread,
};

impl<P: InspectionOps> Controller<P> {
    /// Disassembles the selected thread's stopped snapshot. The thread's
    /// program counter is a known instruction start.
    pub(super) fn disassemble(
        &self,
        stop_id: StopId,
        pid: Pid,
        query: DisassemblyQuery,
    ) -> Result<Disassembly> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_stopped_thread(inferior, pid)?;
        validate_image_current(inferior)?;
        if let DisassemblyRange::Window { before, after, .. } = query.range
            && (before > MAX_WINDOW_BEFORE || after > MAX_WINDOW_AFTER || before + after == 0)
        {
            return Err(Error::InvalidDisassemblyWindow { before, after });
        }
        let target = self.module_image.target();
        let mut decoder = decoder_for(target, query.syntax)?;
        let modules = self.unwind_modules(inferior);
        let mut source = TargetCode {
            ptrace: &self.ptrace,
            pid,
            breakpoints: &inferior.breakpoints,
            modules: &modules,
            program_counter: VirtualAddress::new(self.ptrace.registers(pid)?.rip),
        };

        let view = match query.range {
            DisassemblyRange::Function(address) => {
                let (function, ranges) = function_ranges(&modules, address)?;
                let blocks = Engine::new(&mut source, decoder.as_mut()).function(&ranges)?;
                DisassemblyView::Function {
                    function,
                    blocks: blocks.into(),
                }
            }
            DisassemblyRange::Window {
                address,
                before,
                after,
            } => {
                let window =
                    Engine::new(&mut source, decoder.as_mut()).window(address, before, after)?;
                DisassemblyView::Window {
                    address,
                    boundary: window.boundary,
                    leading: window.shortfall,
                    block: window.block,
                }
            }
        };
        Ok(Disassembly {
            revision: self.revision,
            stop_id,
            target,
            syntax: query.syntax,
            view,
        })
    }
}

/// Resolves the function containing an address to its address ranges in
/// address order: every range of its out-of-line debug-information instance,
/// or else the extent of the code symbol containing it.
fn function_ranges(
    modules: &[UnwindModule<'_>],
    address: VirtualAddress,
) -> Result<(DisassembledFunction, Vec<AddressRange<VirtualAddress>>)> {
    let not_found = || Error::NoFunctionContainsAddress(address);
    let (module, image_address) = unwind_module_for(modules, address).ok_or_else(not_found)?;
    let image = module.image;
    let to_virtual = |range: AddressRange<crate::ImageAddress>| {
        Ok(AddressRange {
            start: module.loaded.virtual_address(range.start)?,
            end: module.loaded.virtual_address(range.end)?,
        })
    };

    let instance = image
        .locate(image_address)
        .physical_instance
        .and_then(|id| image.code_instance(id));
    if let Some(instance) = instance
        && let Some(function) = image.function(instance.function)
    {
        let mut ranges = instance
            .ranges
            .iter()
            .map(|range| to_virtual(*range))
            .collect::<Result<Vec<_>>>()?;
        ranges.sort_by_key(|range| (range.start, range.end));
        return Ok((
            DisassembledFunction {
                module: module.loaded.id,
                name: function.name.clone(),
                origin: FunctionOrigin::DebugInfo {
                    instance: instance.id,
                },
            },
            ranges,
        ));
    }

    let symbol = image.symbolize(image_address).ok_or_else(not_found)?;
    let extent = image
        .symbol(symbol.symbol)
        .and_then(|info| info.extent)
        .expect("a code symbol has an extent");
    Ok((
        DisassembledFunction {
            module: module.loaded.id,
            name: symbol.name,
            origin: FunctionOrigin::Symbol {
                symbol: symbol.symbol,
                provenance: symbol.provenance,
            },
        },
        vec![to_virtual(extent.range)?],
    ))
}

/// The memory and module metadata of one stopped snapshot.
struct TargetCode<'a, P> {
    ptrace: &'a P,
    pid: Pid,
    breakpoints: &'a BTreeMap<VirtualAddress, BreakpointSite>,
    modules: &'a [UnwindModule<'a>],
    program_counter: VirtualAddress,
}

impl<P: InspectionOps> CodeSource for TargetCode<'_, P> {
    fn read(&mut self, address: VirtualAddress, size: usize) -> Result<CodeRead> {
        // Reads never wrap past the end of the address space.
        let room = usize::try_from(u64::MAX - address.get()).unwrap_or(usize::MAX);
        let read = read_logical_memory(
            self.ptrace,
            self.pid,
            self.breakpoints,
            address,
            size.min(room),
        )?;
        let unreadable = match read.completion {
            crate::MemoryReadCompletion::Complete => None,
            crate::MemoryReadCompletion::Incomplete { reason, .. } => Some(reason),
        };
        Ok(CodeRead {
            bytes: read.bytes,
            unreadable,
        })
    }

    fn instruction_starts(
        &self,
        range: AddressRange<VirtualAddress>,
    ) -> BTreeMap<VirtualAddress, BoundaryEvidence> {
        let mut starts = BTreeMap::new();
        let mut insert = |address: VirtualAddress, evidence: BoundaryEvidence| {
            starts
                .entry(address)
                .and_modify(|current: &mut BoundaryEvidence| *current = (*current).min(evidence))
                .or_insert(evidence);
        };
        if range.contains(self.program_counter) {
            insert(self.program_counter, BoundaryEvidence::ProgramCounter);
        }
        for module in self.modules {
            let bias = module.loaded.load_bias;
            let image_range = AddressRange {
                start: crate::ImageAddress::new(range.start.get().saturating_sub(bias)),
                end: crate::ImageAddress::new(range.end.get().saturating_sub(bias)),
            };
            for (address, evidence) in module.image.instruction_starts(image_range) {
                if let Ok(address) = module.loaded.virtual_address(address)
                    && range.contains(address)
                {
                    insert(address, evidence);
                }
            }
        }
        starts
    }

    fn describe(&self, address: VirtualAddress) -> AddressDescription {
        describe_address(self.modules, address)
    }

    fn source_location(&self, address: VirtualAddress) -> Option<SourceLocation> {
        let (module, image_address) = unwind_module_for(self.modules, address)?;
        module.image.source_location(image_address)
    }
}
