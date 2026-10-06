//! The callers of a frame, which recover the values its function's
//! parameters held on entry from the calls that passed them (DWARF 5
//! section 2.5.1.7), through any chain of tail calls between.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use nix::unistd::Pid;

use crate::debug_info::{
    CallSiteId, CallTarget, EntryParameter, VariableRegister, VariableRuntime, VariableRuntimeError,
};
use crate::inspection::InspectionBudget;
use crate::{
    EntryValueUnavailableReason, ImageAddress, ModuleId, SymbolBinding, SymbolKind,
    VariableUnavailableReason, VirtualAddress,
};

use super::frames::{FrameRegisters, PhysicalFrame, PhysicalStack, unwind_module_for};
use super::inspection::LinuxVariableRuntime;
use super::native::InspectionOps;
use super::{Controller, Inferior, RuntimeModule};
use crate::unwind::{DEFAULT_MAX_FRAMES, FrameContext, RegisterFile};

/// How many callers one entry value may consult, through call sites whose
/// values are themselves entry values.
const MAX_ENTRY_VALUE_DEPTH: usize = 16;

/// The frame a runtime reads, as entry values find its caller.
#[derive(Clone, Copy)]
pub(super) struct FrameAt {
    /// The physical activation, innermost first.
    pub(super) activation: usize,
    /// The module describing its code, and its address in that image.
    pub(super) code: Option<(ModuleId, ImageAddress)>,
    /// How many callers away from the inspected frame it is.
    pub(super) depth: usize,
}

/// A stopped thread's activations, unwound once for every entry value one
/// inspection recovers.
pub(super) struct Callers<'a, P: InspectionOps> {
    controller: &'a Controller<P>,
    inferior: &'a Inferior,
    pid: Pid,
    /// The activations unwound so far, and how many were asked for: fewer
    /// means the stack ended.
    stack: RefCell<Option<(PhysicalStack, usize)>>,
}

const fn unavailable(reason: EntryValueUnavailableReason) -> VariableRuntimeError {
    VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(reason))
}

impl<'a, P: InspectionOps> Callers<'a, P> {
    pub(super) fn new(controller: &'a Controller<P>, inferior: &'a Inferior, pid: Pid) -> Rc<Self> {
        Rc::new(Self {
            controller,
            inferior,
            pid,
            stack: RefCell::new(None),
        })
    }

    /// The physical activation at `index`, unwinding further if needed.
    fn activation(
        &self,
        index: usize,
    ) -> Result<Option<(FrameContext, RegisterFile)>, VariableRuntimeError> {
        let mut stack = self.stack.borrow_mut();
        let complete = stack.as_ref().is_some_and(|(stack, requested)| {
            index < stack.frames.len() || stack.frames.len() < *requested
        });
        if !complete {
            let requested = index.saturating_add(4).min(DEFAULT_MAX_FRAMES);
            let unwound = self
                .controller
                .physical_stack(self.inferior, self.pid, requested)
                .map_err(|error| VariableRuntimeError::Fatal(error.to_string().into()))?;
            *stack = Some((unwound, requested));
        }
        Ok(stack
            .as_ref()
            .and_then(|(stack, _)| stack.frames.get(index))
            .map(|frame: &PhysicalFrame| (frame.context.clone(), frame.registers.clone())))
    }

    /// The value `parameter` held on entry to the function of the frame at
    /// `frame`, whose location `module` describes.
    pub(super) fn entry_value(
        self: &Rc<Self>,
        frame: FrameAt,
        module: ModuleId,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let value = self.caller_value(frame, module, parameter, budget);
        if frame.depth == 0 {
            return value;
        }
        // A caller asks for its own entry value to say what it passed, and
        // what its caller lacks is not what the frame's caller lacks.
        value.map_err(|error| match error {
            VariableRuntimeError::Unavailable(
                reason @ VariableUnavailableReason::EntryValue(_),
            ) => unavailable(EntryValueUnavailableReason::Caller(Box::new(reason))),
            error => error,
        })
    }

    /// What the caller of the frame at `frame` passed for `parameter`.
    fn caller_value(
        self: &Rc<Self>,
        frame: FrameAt,
        module: ModuleId,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        if frame.depth >= MAX_ENTRY_VALUE_DEPTH {
            return Err(VariableUnavailableReason::EvaluationLimit.into());
        }
        // Only a frame's own function has entry values.
        let Some((_, code)) = frame.code.filter(|(code_module, _)| *code_module == module) else {
            return Err(unavailable(EntryValueUnavailableReason::NoCaller));
        };
        let frame_module = self.module(module)?;

        let caller_index = frame.activation + 1;
        let Some((context, registers)) = self.activation(caller_index)? else {
            return Err(unavailable(EntryValueUnavailableReason::NoCaller));
        };
        // An interrupted frame did not call the frame above it.
        if context.signal_frame {
            return Err(unavailable(EntryValueUnavailableReason::NoCaller));
        }
        let return_address = context.instruction;
        let modules = self.controller.unwind_modules(self.inferior);
        let lookup = return_address
            .get()
            .checked_sub(1)
            .map(VirtualAddress::new)
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::NoCaller))?;
        let Some((unwind_module, caller_code)) = unwind_module_for(&modules, lookup) else {
            return Err(unavailable(EntryValueUnavailableReason::NoCallSite));
        };
        let caller = self.module(unwind_module.loaded.id)?;
        let caller_return = caller
            .loaded
            .image_address(return_address)
            .map_err(|_| unavailable(EntryValueUnavailableReason::NoCallSite))?;
        let caller_at = FrameAt {
            activation: caller_index,
            code: Some((caller.loaded.id, caller_code)),
            depth: frame.depth + 1,
        };
        let cfa = self
            .controller
            .frame_cfa(self.pid, &modules, caller_at.code, &registers);
        let caller_registers = FrameRegisters::Caller(registers);
        let mut caller_runtime = LinuxVariableRuntime {
            ptrace: &self.controller.ptrace,
            pid: self.pid,
            loaded_module: caller.loaded,
            breakpoints: &self.inferior.breakpoints,
            registers: &caller_registers,
            floating: None,
            cfa,
            tls: caller.tls,
            frame: caller_at,
            callers: Some(Rc::clone(self)),
        };

        let site = caller
            .variables
            .call_site(caller_return, &mut caller_runtime, budget)?
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::NoCallSite))?;
        let target = match site.target {
            CallTarget::Code(address) => caller
                .loaded
                .virtual_address(address)
                .map_err(|error| VariableRuntimeError::Malformed(error.to_string().into()))?,
            CallTarget::Computed(address) => address,
            CallTarget::Symbol(name) => self
                .symbol(&name)
                .ok_or_else(|| unavailable(EntryValueUnavailableReason::UnknownTarget))?,
            CallTarget::Unknown => {
                return Err(unavailable(EntryValueUnavailableReason::UnknownTarget));
            }
        };
        let from = frame_module
            .loaded
            .image_address(target)
            .ok()
            .filter(|address| frame_module.image.contains_address(*address))
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::TargetMismatch))?;
        let path = frame_module.variables.tail_calls(from, code)?;
        let mut chain = Chain {
            callee: frame_module,
            caller,
            site: site.id,
            caller_runtime: &mut caller_runtime,
        };
        chain
            .value(&path, parameter, module, budget)
            .map_err(|error| match error {
                // What the caller cannot provide, the entry value cannot.
                VariableRuntimeError::Unavailable(reason)
                    if !matches!(
                        reason,
                        VariableUnavailableReason::EntryValue(_)
                            | VariableUnavailableReason::EvaluationLimit
                            | VariableUnavailableReason::InspectionLimit(_)
                    ) =>
                {
                    unavailable(EntryValueUnavailableReason::Caller(Box::new(reason)))
                }
                error => error,
            })
    }

    fn module(&self, id: ModuleId) -> Result<&'a RuntimeModule, VariableRuntimeError> {
        self.controller
            .modules
            .get(&id)
            .ok_or_else(|| VariableRuntimeError::Fatal(format!("module {id} is not loaded").into()))
    }

    /// The one address a linker symbol defines a function at, across every
    /// loaded module. An indirect function's callers reach whichever
    /// implementation its resolver chose, which is not its address.
    fn symbol(&self, name: &str) -> Option<VirtualAddress> {
        let mut found = None;
        for module in self.controller.modules.values() {
            for symbol in module.image.symbols_named(name) {
                if symbol.binding == SymbolBinding::Local || symbol.kind != SymbolKind::Function {
                    continue;
                }
                let address = module.loaded.virtual_address(symbol.address).ok()?;
                if found.is_some_and(|found| found != address) {
                    return None;
                }
                found = Some(address);
            }
        }
        found
    }
}

/// The call that entered a frame's function, and the tail calls after it.
struct Chain<'c, 'r, 'a, P: InspectionOps> {
    callee: &'a RuntimeModule,
    caller: &'a RuntimeModule,
    site: CallSiteId,
    caller_runtime: &'c mut LinuxVariableRuntime<'r, P>,
}

impl<P: InspectionOps> Chain<'_, '_, '_, P> {
    /// The value passed for `parameter`, which `requester` describes, by the
    /// last of `links`, the tail calls between the call and the frame, or by
    /// the call itself.
    fn value(
        &mut self,
        links: &[CallSiteId],
        parameter: EntryParameter,
        requester: ModuleId,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let (module, runtime_links) = match links.split_last() {
            None => (self.caller, None),
            Some((last, rest)) => (self.callee, Some((*last, rest))),
        };
        // A parameter entry names a parameter only within its own module.
        if matches!(parameter, EntryParameter::Parameter(_)) && module.loaded.id != requester {
            return Err(unavailable(EntryValueUnavailableReason::NoParameter));
        }
        match runtime_links {
            None => {
                let site = self.site;
                module
                    .variables
                    .call_site_value(site, parameter, self.caller_runtime, budget)
            }
            Some((last, rest)) => module.variables.call_site_value(
                last,
                parameter,
                &mut TailCallFrame {
                    chain: self,
                    links: rest,
                },
                budget,
            ),
        }
    }
}

/// A function that left by a tail call: nothing of its state survives the
/// jump but the values its own caller passed it.
struct TailCallFrame<'f, 'c, 'r, 'a, P: InspectionOps> {
    chain: &'f mut Chain<'c, 'r, 'a, P>,
    links: &'f [CallSiteId],
}

const fn discarded() -> VariableRuntimeError {
    unavailable(EntryValueUnavailableReason::DiscardedState)
}

impl<P: InspectionOps> VariableRuntime for TailCallFrame<'_, '_, '_, '_, P> {
    fn register(&mut self, _register: u16) -> Result<VariableRegister, VariableRuntimeError> {
        Err(discarded())
    }

    fn call_frame_cfa(&self) -> Result<VirtualAddress, VariableRuntimeError> {
        Err(discarded())
    }

    fn tls_address(&mut self, _offset: u64) -> Result<VirtualAddress, VariableUnavailableReason> {
        Err(VariableUnavailableReason::EntryValue(
            EntryValueUnavailableReason::DiscardedState,
        ))
    }

    fn relocate(&self, address: ImageAddress) -> Result<VirtualAddress, Arc<str>> {
        self.chain
            .callee
            .loaded
            .virtual_address(address)
            .map_err(|error| error.to_string().into())
    }

    fn read_memory(
        &mut self,
        _address: VirtualAddress,
        _size: usize,
    ) -> Result<Arc<[u8]>, VariableRuntimeError> {
        Err(discarded())
    }

    fn entry_value(
        &mut self,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let requester = self.chain.callee.loaded.id;
        self.chain.value(self.links, parameter, requester, budget)
    }
}
