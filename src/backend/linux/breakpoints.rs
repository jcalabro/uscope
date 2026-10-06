//! Logical breakpoints and the software trap sites that implement them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use nix::unistd::Pid;

use super::signals::Signal;

use crate::protocol::{
    Breakpoint, BreakpointHit, BreakpointId, BreakpointSpec, ConditionOwner, DebuggerEvent,
    ExecutionId, HitCondition, ResolvedBreakpointLocation, StopReason,
};
use crate::{BreakpointLocation, Error, Result, VirtualAddress};

use super::memory::MemoryAccessError;
use super::native::{LinuxTraceOps, is_vanished_tracee};
use super::{BREAKPOINT_OPCODE, BreakpointOwner, Controller, Inferior, LinuxError, backend_error};

/// The logical breakpoints clients requested, in creation order, counted by
/// spec so that adding one need not compare it with every other. A spec
/// never changes once added; everything else is edited in place.
#[derive(Debug, Default)]
pub(super) struct UserBreakpoints {
    list: Vec<Breakpoint>,
    specs: HashMap<BreakpointSpec, usize>,
}

impl UserBreakpoints {
    pub(super) fn push(&mut self, breakpoint: Breakpoint) {
        *self.specs.entry(breakpoint.spec.clone()).or_default() += 1;
        self.list.push(breakpoint);
    }

    pub(super) fn remove(&mut self, index: usize) -> Breakpoint {
        let breakpoint = self.list.remove(index);
        if let Some(count) = self.specs.get_mut(&breakpoint.spec) {
            *count -= 1;
            if *count == 0 {
                self.specs.remove(&breakpoint.spec);
            }
        }
        breakpoint
    }

    pub(super) fn take(&mut self) -> Vec<Breakpoint> {
        self.specs.clear();
        std::mem::take(&mut self.list)
    }

    /// The breakpoint that a request with this spec and these options would
    /// duplicate.
    fn identical(
        &self,
        spec: &BreakpointSpec,
        options: &crate::BreakpointOptions,
    ) -> Option<&Breakpoint> {
        if !self.specs.contains_key(spec) {
            return None;
        }
        self.list.iter().find(|breakpoint| {
            breakpoint.spec == *spec
                && breakpoint.hit_condition == options.hit_condition
                && breakpoint.condition == options.condition
                && breakpoint.log_message == options.log_message
        })
    }
}

impl Deref for UserBreakpoints {
    type Target = [Breakpoint];

    fn deref(&self) -> &[Breakpoint] {
        &self.list
    }
}

impl DerefMut for UserBreakpoints {
    fn deref_mut(&mut self) -> &mut [Breakpoint] {
        &mut self.list
    }
}

impl<'a> IntoIterator for &'a UserBreakpoints {
    type Item = &'a Breakpoint;
    type IntoIter = std::slice::Iter<'a, Breakpoint>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter()
    }
}

impl<'a> IntoIterator for &'a mut UserBreakpoints {
    type Item = &'a mut Breakpoint;
    type IntoIter = std::slice::IterMut<'a, Breakpoint>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter_mut()
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Adds a logical breakpoint. Its traps are installed at once when the
    /// inferior's sites are live, which requires every thread to be stopped;
    /// a launching or attaching inferior installs them at its first stop.
    pub(super) fn add_breakpoint(
        &mut self,
        spec: BreakpointSpec,
        options: crate::BreakpointOptions,
    ) -> Result<Breakpoint> {
        if let Some(existing) = self.breakpoints.identical(&spec, &options) {
            return Ok(existing.clone());
        }

        let id = BreakpointId::new(self.next_breakpoint_id);
        let next_id = self
            .next_breakpoint_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::BreakpointIdExhausted))?;
        let breakpoint = Breakpoint {
            hit_condition: options.hit_condition,
            condition: options.condition,
            log_message: options.log_message,
            ..self.resolve_breakpoint(id, spec, options.pending)?
        };

        if self.sites_live() {
            let inferior = self.inferior.as_mut().expect("live sites have an inferior");
            install_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
            self.step_over_where_threads_trapped(&breakpoint)?;
        }

        self.next_breakpoint_id = next_id;
        self.breakpoints.push(breakpoint.clone());
        self.publish_breakpoints_changed();
        Ok(breakpoint)
    }

    pub(super) fn resolve_breakpoint(
        &self,
        id: BreakpointId,
        spec: BreakpointSpec,
        pending: bool,
    ) -> Result<Breakpoint> {
        let locations = match &spec {
            BreakpointSpec::Address(address) => Arc::from([ResolvedBreakpointLocation {
                location: BreakpointLocation::Virtual(*address),
                code_instances: Arc::from([]),
                library: None,
            }]),
            _ => match self.resolve_in_modules(&spec) {
                Ok(locations) => locations,
                Err(Error::FunctionNotFound(_) | Error::SourceFileNotFound(_)) if pending => {
                    Arc::from([])
                }
                Err(error) => return Err(error),
            },
        };
        Ok(Breakpoint {
            id,
            spec,
            locations,
            hit_condition: None,
            condition: None,
            log_message: None,
            hit_count: 0,
        })
    }

    /// Resolves a function or source spec in the program and every loaded
    /// shared library. The spec fails only when no module has code for it;
    /// a module that does not know the name or file contributes nothing.
    pub(super) fn resolve_in_modules(
        &self,
        spec: &BreakpointSpec,
    ) -> Result<Arc<[ResolvedBreakpointLocation]>> {
        let mut locations = Vec::new();
        let mut failure = None;
        let main = (crate::ModuleId::new(0), &*self.module_image, None);
        let libraries = self
            .modules
            .values()
            .filter(|module| module.loaded.id != crate::ModuleId::new(0))
            .map(|module| {
                (
                    module.loaded.id,
                    &*module.image,
                    Some(module.loaded.load_bias),
                )
            });
        for (module, image, bias) in std::iter::once(main).chain(libraries) {
            match resolve_in_image(image, spec) {
                Ok(addresses) => {
                    for (address, code_instances) in addresses {
                        locations.push(ResolvedBreakpointLocation {
                            location: match bias {
                                None => BreakpointLocation::Image(address),
                                Some(bias) => BreakpointLocation::Virtual(VirtualAddress::new(
                                    bias.checked_add(address.get())
                                        .ok_or(Error::AddressOverflow)?,
                                )),
                            },
                            code_instances,
                            library: bias.map(|_| module),
                        });
                    }
                }
                // A module without the name or file is not a failure, but
                // one with the file and no code at the line is, unless
                // another module has code for it.
                Err(error @ (Error::FunctionNotFound(_) | Error::SourceFileNotFound(_))) => {
                    failure.get_or_insert(error);
                }
                Err(error) => {
                    if failure.as_ref().is_none_or(|failure| {
                        matches!(
                            failure,
                            Error::FunctionNotFound(_) | Error::SourceFileNotFound(_)
                        )
                    }) {
                        failure = Some(error);
                    }
                }
            }
        }
        if locations.is_empty() {
            return Err(failure.expect("the program itself was searched"));
        }
        Ok(locations.into())
    }

    /// A thread that reported a trap at a site steps over the trap there
    /// when it moves on, whichever breakpoint owns the site by then: its
    /// arrival there was counted. Removing the breakpoint it hit forgets the
    /// step, so one added there again before the thread moves restores it.
    /// Any other thread standing at the new breakpoint arrives there when it
    /// resumes, as one does after a step that ends at a breakpoint.
    fn step_over_where_threads_trapped(&mut self, breakpoint: &Breakpoint) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let sites = breakpoint
            .locations
            .iter()
            .map(|resolved| runtime_breakpoint_address(inferior, resolved.location))
            .collect::<Result<BTreeSet<_>>>()?;
        for thread in inferior.threads.values_mut() {
            if thread.stopped_at_breakpoint.is_none()
                && let Some(address) = thread.trapped_at.filter(|address| sites.contains(address))
            {
                thread.stopped_at_breakpoint = Some(address);
            }
        }
        Ok(())
    }

    pub(super) fn remove_breakpoint(&mut self, id: BreakpointId) -> Result<Breakpoint> {
        let index = self
            .breakpoints
            .iter()
            .position(|breakpoint| breakpoint.id == id)
            .ok_or(Error::BreakpointNotFound(id.get()))?;
        let breakpoint = self.breakpoints[index].clone();
        if self.sites_live() {
            let inferior = self.inferior.as_mut().expect("live sites have an inferior");
            remove_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
        }
        self.breakpoints.remove(index);
        self.publish_breakpoints_changed();
        Ok(breakpoint)
    }

    pub(super) fn remove_all_breakpoints(&mut self) -> Result<Arc<[Breakpoint]>> {
        if self.breakpoints.is_empty() {
            return Ok(Arc::from([]));
        }
        if self.sites_live() {
            let inferior = self.inferior.as_mut().expect("live sites have an inferior");
            let stopped_at = inferior
                .threads
                .iter()
                .map(|(&pid, thread)| (pid, thread.stopped_at_breakpoint))
                .collect::<Vec<_>>();
            let mut removed = Vec::new();
            for breakpoint in &self.breakpoints {
                if let Err(cause) = remove_logical_breakpoint(&self.ptrace, inferior, breakpoint) {
                    for prior in removed.iter().rev() {
                        if let Err(recovery) =
                            install_logical_breakpoint(&self.ptrace, inferior, prior)
                        {
                            return Err(backend_error(LinuxError::BreakpointRemoveRecovery {
                                cause: cause.to_string(),
                                recovery: recovery.to_string(),
                            }));
                        }
                    }
                    for (pid, address) in stopped_at {
                        inferior.thread_mut(pid)?.stopped_at_breakpoint = address;
                    }
                    return Err(cause);
                }
                removed.push(breakpoint.clone());
            }
        }
        let removed: Arc<[Breakpoint]> = self.breakpoints.take().into();
        self.publish_breakpoints_changed();
        Ok(removed)
    }

    /// Replaces a breakpoint's hit condition, keeping the hits it counted.
    pub(super) fn set_breakpoint_hit_condition(
        &mut self,
        id: BreakpointId,
        hit_condition: Option<HitCondition>,
    ) -> Result<Breakpoint> {
        self.edit_breakpoint(id, |breakpoint| breakpoint.hit_condition = hit_condition)
    }

    pub(super) fn set_breakpoint_condition(
        &mut self,
        id: BreakpointId,
        condition: Option<crate::Condition>,
    ) -> Result<Breakpoint> {
        self.edit_breakpoint(id, |breakpoint| breakpoint.condition = condition)
    }

    /// Changes controller state only, so no stop is required.
    fn edit_breakpoint(
        &mut self,
        id: BreakpointId,
        edit: impl FnOnce(&mut Breakpoint),
    ) -> Result<Breakpoint> {
        let breakpoint = self
            .breakpoints
            .iter_mut()
            .find(|breakpoint| breakpoint.id == id)
            .ok_or(Error::BreakpointNotFound(id.get()))?;
        edit(breakpoint);
        let breakpoint = breakpoint.clone();
        self.publish_breakpoints_changed();
        Ok(breakpoint)
    }

    /// Counts one hit for every logical breakpoint owning the site at
    /// `address` that thread `pid` reached, and returns those the hit stops
    /// at: its hit condition and condition are met, and it logs no message
    /// instead. A condition that cannot be evaluated stops, as gdb does,
    /// since skipping the hit could hide what the user asked to see.
    pub(super) fn record_breakpoint_hits(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Arc<[BreakpointHit]> {
        let Some(site) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.breakpoints.get(&address))
        else {
            return Arc::from([]);
        };
        let owners = site
            .owners
            .iter()
            .filter_map(|owner| match owner {
                BreakpointOwner::User(id) => Some(*id),
                BreakpointOwner::Plan(_) | BreakpointOwner::Loader => None,
            })
            .collect::<BTreeSet<_>>();
        let mut candidates = Vec::new();
        for breakpoint in &mut self.breakpoints {
            if !owners.contains(&breakpoint.id) {
                continue;
            }
            breakpoint.hit_count = breakpoint.hit_count.saturating_add(1);
            if breakpoint
                .hit_condition
                .is_none_or(|condition| condition.is_met(breakpoint.hit_count))
            {
                candidates.push((
                    BreakpointHit {
                        breakpoint: breakpoint.id,
                        hit_count: breakpoint.hit_count,
                    },
                    breakpoint.condition.clone(),
                    breakpoint.log_message.clone(),
                ));
            }
        }
        let reason = StopReason::Breakpoint {
            address,
            hits: Arc::from([]),
        };
        let mut stopping = Vec::new();
        for (hit, condition, log_message) in candidates {
            if let Some(condition) = condition {
                let owner = ConditionOwner::Breakpoint(hit.breakpoint);
                match self.judge_condition(pid, &condition, &reason, owner) {
                    Some(true) => {}
                    Some(false) => continue,
                    // A breakpoint that logs stops too.
                    None => {
                        stopping.push(hit);
                        continue;
                    }
                }
            }
            if let Some(message) = log_message {
                let parts = self.log_parts(pid, &message, &reason);
                self.publish_hit_event(pid, |revision, process_id, thread_id| {
                    DebuggerEvent::LogMessage {
                        revision,
                        process_id,
                        thread_id,
                        breakpoint: hit.breakpoint,
                        parts,
                    }
                });
                continue;
            }
            stopping.push(hit);
        }
        stopping.sort_unstable_by_key(|hit| hit.breakpoint);
        stopping.into()
    }

    /// Whether `condition` holds in the innermost frame of thread `pid`, as
    /// a hit's stop `reason` presents it, or `None` when it cannot be
    /// evaluated, which is published. Such a hit stops, as gdb does, since
    /// skipping it could hide what the user asked to see. A thread SIGKILL
    /// took out of its stop meanwhile stops too, and its exit, not the
    /// condition, is what clients hear of.
    pub(super) fn judge_condition(
        &mut self,
        pid: Pid,
        condition: &crate::Condition,
        reason: &StopReason,
        owner: ConditionOwner,
    ) -> Option<bool> {
        match self.condition_met(pid, condition, reason) {
            Ok(holds) => Some(holds),
            Err(None) => None,
            Err(Some(error)) => {
                self.publish_hit_event(pid, |revision, process_id, thread_id| {
                    DebuggerEvent::ConditionFailed {
                        revision,
                        process_id,
                        thread_id,
                        owner,
                        error: error.into(),
                    }
                });
                None
            }
        }
    }

    /// Evaluates a condition in the innermost frame of a thread stopped at a
    /// hit: its truth, or why it has none, or `Err(None)` when the thread
    /// left its stop.
    fn condition_met(
        &self,
        pid: Pid,
        condition: &crate::Condition,
        reason: &StopReason,
    ) -> std::result::Result<bool, Option<String>> {
        let expression = condition.expression();
        match self.evaluate_at_hit(pid, expression, true, reason) {
            Ok(crate::Evaluation::Value { value, cause }) => match value.state {
                crate::VariableState::Available {
                    value: crate::VariableValue::Scalar(crate::ScalarValue::Boolean(holds)),
                    ..
                } => Ok(holds),
                crate::VariableState::Unavailable(reason) => Err(Some(cause.map_or_else(
                    || format!("the condition is unavailable: {reason}"),
                    |cause| {
                        format!(
                            "`{}` is unavailable: {reason}",
                            cause.text(expression.text())
                        )
                    },
                ))),
                state => Err(Some(format!("the condition has no truth value: {state:?}"))),
            },
            Ok(_) => Err(Some("the condition has no truth value".to_owned())),
            Err(error) if is_vanished_tracee(&error) => Err(None),
            Err(error) => Err(Some(error.to_string())),
        }
    }

    /// Reads the values a log message shows, as the hitting thread sees them.
    fn log_parts(
        &self,
        pid: Pid,
        message: &crate::LogMessage,
        reason: &StopReason,
    ) -> Arc<[crate::LogPart]> {
        message
            .segments()
            .iter()
            .map(|segment| match segment {
                crate::LogSegment::Text(text) => crate::LogPart::Text(Arc::clone(text)),
                crate::LogSegment::Value(expression) => {
                    match self.evaluate_at_hit(pid, expression, false, reason) {
                        Ok(crate::Evaluation::Value { value, .. }) => crate::LogPart::Value {
                            expression: expression.clone(),
                            type_info: value.type_info,
                            state: value.state,
                        },
                        Ok(_) => crate::LogPart::Error {
                            expression: expression.clone(),
                            error: "a range cannot be logged".into(),
                        },
                        Err(error) => crate::LogPart::Error {
                            expression: expression.clone(),
                            error: error.to_string().into(),
                        },
                    }
                }
            })
            .collect()
    }

    fn publish_hit_event(
        &mut self,
        pid: Pid,
        event: impl FnOnce(u64, crate::ProcessId, crate::ThreadId) -> DebuggerEvent,
    ) {
        let Some(process_id) = self
            .inferior
            .as_ref()
            .map(|inferior| super::process_id(inferior.tgid))
        else {
            return;
        };
        self.bump_revision();
        let _ = self.events.send(event(
            self.revision,
            process_id,
            super::debug_thread_id(pid),
        ));
    }

    /// Starts every breakpoint's count again for a new process.
    pub(super) fn reset_breakpoint_hit_counts(&mut self) {
        for breakpoint in &mut self.breakpoints {
            breakpoint.hit_count = 0;
        }
    }

    pub(super) fn publish_breakpoints_changed(&mut self) {
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });
    }

    pub(super) fn remove_breakpoint_owner(
        &mut self,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
    }

    pub(super) fn cleanup_plan_breakpoints(&mut self, execution: ExecutionId) -> Result<()> {
        let owner = BreakpointOwner::Plan(execution);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(recorded) = inferior.plan_sites.get(&execution) else {
            return Ok(());
        };
        let addresses = recorded
            .iter()
            .copied()
            .filter(|address| {
                inferior
                    .breakpoints
                    .get(address)
                    .is_some_and(|site| site.owners.contains(&owner))
            })
            .collect::<Vec<_>>();

        for address in addresses {
            self.remove_breakpoint_owner(address, owner)?;
        }
        self.inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .plan_sites
            .remove(&execution);
        Ok(())
    }

    pub(super) fn install_additional_plan_breakpoints(
        &mut self,
        execution: ExecutionId,
        addresses: &BTreeSet<VirtualAddress>,
    ) -> Result<()> {
        let owner = BreakpointOwner::Plan(execution);
        let new_addresses = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            addresses
                .iter()
                .copied()
                .filter(|address| {
                    inferior
                        .breakpoints
                        .get(address)
                        .is_none_or(|site| !site.owners.contains(&owner))
                })
                .collect::<Vec<_>>()
        };
        let mut installed = Vec::new();
        for address in new_addresses {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            if let Err(error) = install_plan_breakpoint(&self.ptrace, inferior, address, execution)
            {
                if self.lost_to_sigkill(&error) {
                    return Err(error);
                }
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                for address in installed.into_iter().rev() {
                    if let Err(recovery) =
                        remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
                    {
                        let _ = self.ptrace.kill(inferior.tgid, Signal::SIGKILL);
                        return Err(backend_error(LinuxError::ResumeRecovery {
                            cause: error.to_string(),
                            recovery: recovery.to_string(),
                        }));
                    }
                }
                return Err(error);
            }
            installed.push(address);
        }
        Ok(())
    }

    pub(super) fn restore_active_breakpoints(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let removed: Vec<_> = inferior
            .repairs
            .iter()
            .filter(|group| group.site_removed)
            .map(|group| group.address)
            .collect();
        let pid = inferior.memory_thread();
        for address in removed {
            self.ptrace
                .reinstall_breakpoint(pid, &mut inferior.breakpoints, address)?;
        }
        inferior.repairs.clear();
        Ok(())
    }

    /// Forgets every site whose memory no longer holds its trap, after the
    /// process may have changed its mappings. Code that moved, as the vDSO
    /// does under mremap(2), carries its traps along, so each is taken out
    /// where it went; a function breakpoint is installed there again when
    /// it is resolved. A site whose memory went away, or now holds
    /// something else, is never written. Each owner loses the site: a
    /// breakpoint its location, a plan its site, and the loader its hook.
    /// Returns whether any breakpoint lost a location.
    pub(super) fn reconcile_sites(&mut self, moved: &[MovedCode]) -> Result<bool> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(false);
        };
        let pid = inferior.memory_thread();
        let mut lost = Vec::new();
        // Traps come out of moved code first, since one may have landed on
        // another site's address.
        for (&address, site) in &inferior.breakpoints {
            let Some(code) = moved
                .iter()
                .find(|code| site.installed && code.from.contains(&address.get()))
            else {
                continue;
            };
            let destination = code.to + (address.get() - code.from.start);
            if site.original_byte != BREAKPOINT_OPCODE
                && memory_byte(&self.ptrace, pid, destination)? == Some(BREAKPOINT_OPCODE)
            {
                let word = self.ptrace.read_word(pid, destination)?;
                self.ptrace.write_word(
                    pid,
                    destination,
                    (word & !0xff) | u64::from(site.original_byte),
                )?;
            }
            lost.push(address);
        }
        for (&address, site) in &inferior.breakpoints {
            if !site.installed || lost.contains(&address) {
                continue;
            }
            let byte = memory_byte(&self.ptrace, pid, address.get())?;
            // A trap over a trap cannot tell whether the memory changed.
            if byte != Some(BREAKPOINT_OPCODE)
                && (byte.is_none() || site.original_byte != BREAKPOINT_OPCODE)
            {
                lost.push(address);
            }
        }

        let mut lost_locations = Vec::new();
        for address in lost {
            let site = inferior
                .breakpoints
                .remove(&address)
                .expect("lost site existed");
            forget_removed_site(inferior, address, site.original_byte);
            for owner in site.owners {
                match owner {
                    BreakpointOwner::User(id) => lost_locations.push((id, address)),
                    BreakpointOwner::Plan(execution) => {
                        if let Some(sites) = inferior.plan_sites.get_mut(&execution) {
                            sites.remove(&address);
                        }
                    }
                    BreakpointOwner::Loader => inferior.loader_site = None,
                }
            }
        }
        for &(id, address) in &lost_locations {
            let Some(breakpoint) = self
                .breakpoints
                .iter_mut()
                .find(|breakpoint| breakpoint.id == id)
            else {
                continue;
            };
            breakpoint.locations = breakpoint
                .locations
                .iter()
                .filter(|location| {
                    runtime_breakpoint_address(inferior, location.location).ok() != Some(address)
                })
                .cloned()
                .collect();
        }
        Ok(!lost_locations.is_empty())
    }
}

/// Module code that moved since the last stop: the bytes at `from` are now
/// at `to` onwards, the debugger's traps among them.
#[derive(Debug)]
pub(super) struct MovedCode {
    pub(super) from: std::ops::Range<u64>,
    pub(super) to: u64,
}

/// The byte at `address`, or `None` where no memory is mapped.
fn memory_byte(ptrace: &impl LinuxTraceOps, pid: Pid, address: u64) -> Result<Option<u8>> {
    match ptrace.read_memory_word(pid, address) {
        Ok(word)
        | Err(MemoryAccessError::Partial {
            word,
            readable: 1..,
        }) => Ok(Some(word.to_ne_bytes()[0])),
        Err(MemoryAccessError::Inaccessible | MemoryAccessError::Partial { .. }) => Ok(None),
        Err(MemoryAccessError::Fatal(error)) => Err(error),
    }
}

/// Resolves a function or source spec in one module image to image
/// addresses and the code instances at each.
fn resolve_in_image(
    image: &crate::ModuleImage,
    spec: &BreakpointSpec,
) -> Result<Vec<(crate::ImageAddress, Arc<[crate::CodeInstanceId]>)>> {
    match spec {
        BreakpointSpec::Address(_) => Ok(Vec::new()),
        BreakpointSpec::Function(name) => {
            // Like gdb, break at every function with the name that has
            // code: overloads and same-named static functions alike.
            // A function the image only declares, such as one another
            // module defines, has no code here.
            let defined = image
                .functions_named(name)
                .filter(|function| image.instances_for_function(function.id).next().is_some())
                .collect::<Vec<_>>();
            if defined.is_empty() {
                return symbol_locations(image, name);
            }
            function_locations(image, defined)
        }
        BreakpointSpec::FileFunction { path, function } => {
            let source = image.source_file_matching(path)?;
            let functions = image
                .functions()
                .iter()
                .filter(|candidate| candidate.name.as_ref() == function)
                .filter(|candidate| {
                    candidate
                        .declaration
                        .as_ref()
                        .is_some_and(|location| location.file == source.id)
                })
                .collect::<Vec<_>>();
            if functions.is_empty() {
                return Err(Error::FunctionNotFound(function.clone()));
            }
            function_locations(image, functions)
        }
        BreakpointSpec::Source { path, line } => {
            let source = image.source_file_matching(path)?;
            let unavailable = || Error::SourceLineUnavailable {
                path: path.clone(),
                line: line.get(),
            };
            let line = image
                .breakpoint_line(source.id, *line)
                .ok_or_else(unavailable)?;
            let mut addresses = image
                .statement_addresses(source.id, line)
                .collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(unavailable());
            }
            addresses.sort_unstable();
            // Like gdb, stop where the line begins in each instance of its
            // code, not again at its later statements, such as the use of
            // a call's result or a loop's condition.
            let mut seen = BTreeSet::new();
            let mut locations = Vec::new();
            for address in addresses {
                let code_instances = image
                    .code_instances()
                    .iter()
                    .filter(|instance| instance.contains(address))
                    .collect::<Vec<_>>();
                let innermost = code_instances
                    .iter()
                    .find(|instance| {
                        !code_instances
                            .iter()
                            .any(|other| other.parent == Some(instance.id))
                    })
                    .map(|instance| instance.id);
                // The line is the innermost instance's code, so a stop there
                // presents that instance's frame.
                if innermost.is_none_or(|instance| seen.insert(instance)) {
                    locations.push((address, innermost.into_iter().collect()));
                }
            }
            Ok(locations)
        }
    }
}

/// The entries of the code symbols with a name, for an image whose debug
/// information does not describe the function, such as a system library's.
/// An indirect function's symbol names its resolver, not the function.
fn symbol_locations(
    image: &crate::ModuleImage,
    name: &str,
) -> Result<Vec<(crate::ImageAddress, Arc<[crate::CodeInstanceId]>)>> {
    let entries = image
        .symbols()
        .iter()
        .filter(|symbol| {
            &*symbol.name == name
                && symbol.kind == crate::SymbolKind::Function
                && symbol.extent.is_some()
        })
        .map(|symbol| symbol.address)
        .collect::<BTreeSet<_>>();
    if entries.is_empty() {
        return Err(Error::FunctionNotFound(name.to_owned()));
    }
    Ok(entries
        .into_iter()
        .map(|address| (address, Arc::from([])))
        .collect())
}

/// The locations of every instance of some functions: one per address
/// where an instance's code begins after its prologue.
fn function_locations<'a>(
    image: &crate::ModuleImage,
    functions: impl IntoIterator<Item = &'a crate::FunctionInfo>,
) -> Result<Vec<(crate::ImageAddress, Arc<[crate::CodeInstanceId]>)>> {
    let mut instances = Vec::new();
    for function in functions {
        instances.extend(image.instances_for_function(function.id));
    }
    if instances.is_empty()
        || instances.iter().any(|instance| {
            image
                .recommended_entries_for_instance(instance.id)
                .next()
                .is_none()
        })
    {
        return Err(Error::LocationUnavailable);
    }
    let mut by_address = BTreeMap::<_, Vec<_>>::new();
    for instance in instances {
        for entry in image.recommended_entries_for_instance(instance.id) {
            by_address
                .entry(entry.address)
                .or_default()
                .push(instance.id);
        }
    }
    Ok(by_address
        .into_iter()
        .map(|(address, code_instances)| (address, code_instances.into()))
        .collect())
}

pub(super) fn runtime_breakpoint_address(
    inferior: &Inferior,
    location: BreakpointLocation,
) -> Result<VirtualAddress> {
    match location {
        BreakpointLocation::Image(address) => inferior.loaded_module.virtual_address(address),
        BreakpointLocation::Virtual(address) => Ok(address),
    }
}

/// Installs a site that `execution`'s plan owns, and records it for the
/// plan's cleanup. The record comes first, so even a failed install is
/// visited.
pub(super) fn install_plan_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    address: VirtualAddress,
    execution: ExecutionId,
) -> Result<()> {
    inferior
        .plan_sites
        .entry(execution)
        .or_default()
        .insert(address);
    let pid = inferior.memory_thread();
    ptrace.install_breakpoint(
        pid,
        &mut inferior.breakpoints,
        address,
        BreakpointOwner::Plan(execution),
    )
}

pub(super) fn install_logical_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    breakpoint: &Breakpoint,
) -> Result<()> {
    let owner = BreakpointOwner::User(breakpoint.id);
    let addresses = breakpoint
        .locations
        .iter()
        .map(|resolved| runtime_breakpoint_address(inferior, resolved.location))
        .collect::<Result<Vec<_>>>()?;
    let mut installed = Vec::with_capacity(addresses.len());

    for address in addresses {
        let pid = inferior.memory_thread();
        if let Err(cause) =
            ptrace.install_breakpoint(pid, &mut inferior.breakpoints, address, owner)
        {
            for installed_address in installed.into_iter().rev() {
                if let Err(recovery) =
                    remove_breakpoint_owner_from(ptrace, inferior, installed_address, owner)
                {
                    return Err(backend_error(LinuxError::BreakpointInstallRecovery {
                        cause: cause.to_string(),
                        recovery: recovery.to_string(),
                    }));
                }
            }

            return Err(cause);
        }
        installed.push(address);
    }

    Ok(())
}

pub(super) fn remove_logical_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    breakpoint: &Breakpoint,
) -> Result<()> {
    let owner = BreakpointOwner::User(breakpoint.id);
    let addresses = breakpoint
        .locations
        .iter()
        .map(|resolved| runtime_breakpoint_address(inferior, resolved.location))
        .collect::<Result<Vec<_>>>()?;

    for &address in &addresses {
        let site = inferior
            .breakpoints
            .get(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        if !site.owners.contains(&owner) {
            return Err(backend_error(LinuxError::BreakpointOwnerMissing(address)));
        }
    }

    let mut removed = Vec::new();
    for address in addresses {
        if let Err(cause) = remove_breakpoint_owner_from(ptrace, inferior, address, owner) {
            for prior in removed.into_iter().rev() {
                if let Err(recovery) = ptrace.install_breakpoint(
                    inferior.memory_thread(),
                    &mut inferior.breakpoints,
                    prior,
                    owner,
                ) {
                    return Err(backend_error(LinuxError::BreakpointRemoveRecovery {
                        cause: cause.to_string(),
                        recovery: recovery.to_string(),
                    }));
                }
            }
            return Err(cause);
        }
        removed.push(address);
    }
    Ok(())
}

/// Forgets every repair of a site whose original instruction is restored,
/// remembering the site for processes forked while it was installed.
pub(super) fn forget_removed_site(
    inferior: &mut Inferior,
    address: VirtualAddress,
    original_byte: u8,
) {
    inferior
        .former_sites
        .entry(address)
        .or_insert(original_byte);
    // With the original instruction restored, no thread needs a repair step.
    for thread in inferior.threads.values_mut() {
        if thread.stopped_at_breakpoint == Some(address) {
            thread.stopped_at_breakpoint = None;
        }
    }
    inferior.repairs.retain(|group| group.address != address);
}

pub(super) fn remove_breakpoint_owner_from(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    address: VirtualAddress,
    owner: BreakpointOwner,
) -> Result<()> {
    let remove_site = {
        let site = inferior
            .breakpoints
            .get_mut(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        if !site.owners.contains(&owner) {
            return Err(backend_error(LinuxError::BreakpointOwnerMissing(address)));
        }
        site.owners.len() == 1
    };

    if remove_site {
        let pid = inferior.memory_thread();
        ptrace.remove_breakpoint(pid, &mut inferior.breakpoints, address)?;
        let removed = inferior
            .breakpoints
            .remove(&address)
            .expect("empty breakpoint site existed");
        forget_removed_site(inferior, address, removed.original_byte);
    } else {
        let removed = inferior
            .breakpoints
            .get_mut(&address)
            .expect("known breakpoint site")
            .owners
            .remove(&owner);
        assert!(removed, "known breakpoint owner existed");
    }
    Ok(())
}
