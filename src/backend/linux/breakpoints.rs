//! Logical breakpoints and the software trap sites that implement them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nix::sys::signal::Signal as NixSignal;

use crate::protocol::{
    Breakpoint, BreakpointId, BreakpointSpec, DebuggerEvent, ExecutionId,
    ResolvedBreakpointLocation,
};
use crate::{BreakpointLocation, Error, Result, VirtualAddress};

use super::native::LinuxTraceOps;
use super::{
    BreakpointOwner, Controller, Inferior, LinuxError, backend_error, validate_public_stop,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn add_breakpoint(&mut self, spec: BreakpointSpec) -> Result<Breakpoint> {
        if let Some(existing) = self
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.spec == spec)
        {
            return Ok(existing.clone());
        }

        let id = BreakpointId::new(self.next_breakpoint_id);
        let next_id = self
            .next_breakpoint_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::BreakpointIdExhausted))?;
        let breakpoint = self.resolve_breakpoint(id, spec)?;

        if let Some(inferior) = self.inferior.as_mut() {
            validate_public_stop(inferior, inferior.public_stop.as_ref().map(|stop| stop.id))?;
            install_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
        }

        self.next_breakpoint_id = next_id;
        self.breakpoints.push(breakpoint.clone());
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });

        Ok(breakpoint)
    }

    pub(super) fn resolve_breakpoint(
        &self,
        id: BreakpointId,
        spec: BreakpointSpec,
    ) -> Result<Breakpoint> {
        let locations: Arc<[ResolvedBreakpointLocation]> = match &spec {
            BreakpointSpec::Address(address) => Arc::from([ResolvedBreakpointLocation {
                location: BreakpointLocation::Virtual(*address),
                code_instances: Arc::from([]),
            }]),
            BreakpointSpec::Function(name) => self.resolve_function_breakpoint(std::iter::once(
                self.module_image.function_named(name)?,
            ))?,
            BreakpointSpec::FileFunction { path, function } => {
                let source = self.module_image.source_file_matching(path)?;
                let functions = self
                    .module_image
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
                self.resolve_function_breakpoint(functions)?
            }
            BreakpointSpec::Source { path, line } => {
                let source = self.module_image.source_file_matching(path)?;
                let addresses = self
                    .module_image
                    .statement_addresses(source.id, *line)
                    .collect::<Vec<_>>();
                if addresses.is_empty() {
                    return Err(Error::SourceLineUnavailable {
                        path: path.clone(),
                        line: line.get(),
                    });
                }
                addresses
                    .into_iter()
                    .map(|address| {
                        let code_instances = self
                            .module_image
                            .code_instances()
                            .iter()
                            .filter(|instance| instance.contains(address))
                            .map(|instance| instance.id)
                            .collect::<Vec<_>>()
                            .into();
                        ResolvedBreakpointLocation {
                            location: BreakpointLocation::Image(address),
                            code_instances,
                        }
                    })
                    .collect::<Vec<_>>()
                    .into()
            }
        };

        Ok(Breakpoint {
            id,
            spec,
            locations,
        })
    }

    pub(super) fn resolve_function_breakpoint<'a>(
        &self,
        functions: impl IntoIterator<Item = &'a crate::FunctionInfo>,
    ) -> Result<Arc<[ResolvedBreakpointLocation]>> {
        let mut instances = Vec::new();
        for function in functions {
            instances.extend(self.module_image.instances_for_function(function.id));
        }
        if instances.is_empty()
            || instances.iter().any(|instance| {
                self.module_image
                    .recommended_entries_for_instance(instance.id)
                    .next()
                    .is_none()
            })
        {
            return Err(Error::LocationUnavailable);
        }

        let mut by_address = BTreeMap::<_, Vec<_>>::new();
        for instance in instances {
            for entry in self
                .module_image
                .recommended_entries_for_instance(instance.id)
            {
                by_address
                    .entry(entry.address)
                    .or_default()
                    .push(instance.id);
            }
        }

        Ok(by_address
            .into_iter()
            .map(|(address, code_instances)| ResolvedBreakpointLocation {
                location: BreakpointLocation::Image(address),
                code_instances: code_instances.into(),
            })
            .collect::<Vec<_>>()
            .into())
    }

    pub(super) fn remove_breakpoint(&mut self, id: BreakpointId) -> Result<Breakpoint> {
        let index = self
            .breakpoints
            .iter()
            .position(|breakpoint| breakpoint.id == id)
            .ok_or(Error::BreakpointNotFound(id.get()))?;
        let breakpoint = self.breakpoints[index].clone();
        if let Some(inferior) = self.inferior.as_mut() {
            validate_public_stop(inferior, None)?;
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
        if let Some(inferior) = self.inferior.as_mut() {
            validate_public_stop(inferior, None)?;
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
        let removed: Arc<[Breakpoint]> = std::mem::take(&mut self.breakpoints).into();
        self.publish_breakpoints_changed();
        Ok(removed)
    }

    pub(super) fn publish_breakpoints_changed(&mut self) {
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn remove_breakpoint_owner(
        &mut self,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
    }

    pub(super) fn cleanup_plan_breakpoints(&mut self, execution: ExecutionId) -> Result<()> {
        let addresses = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .breakpoints
            .iter()
            .filter_map(|(&address, site)| {
                site.owners
                    .contains(&BreakpointOwner::Plan(execution))
                    .then_some(address)
            })
            .collect::<Vec<_>>();

        for address in addresses {
            self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;
        }
        Ok(())
    }
}

impl<P: LinuxTraceOps> Controller<P> {
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
            if let Err(error) = self.ptrace.install_breakpoint(
                inferior.tgid,
                &mut inferior.breakpoints,
                address,
                owner,
            ) {
                for address in installed.into_iter().rev() {
                    if let Err(recovery) =
                        remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
                    {
                        let _ = self.ptrace.kill(inferior.tgid, NixSignal::SIGKILL);
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
        for address in removed {
            self.ptrace
                .reinstall_breakpoint(inferior.tgid, &mut inferior.breakpoints, address)?;
        }
        inferior.repairs.clear();
        Ok(())
    }
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
        if let Err(cause) =
            ptrace.install_breakpoint(inferior.tgid, &mut inferior.breakpoints, address, owner)
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
                    inferior.tgid,
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
    let removed_sites = removed
        .iter()
        .copied()
        .filter(|address| !inferior.breakpoints.contains_key(address))
        .collect::<BTreeSet<_>>();
    for thread in inferior.threads.values_mut() {
        if thread
            .stopped_at_breakpoint
            .is_some_and(|address| removed_sites.contains(&address))
        {
            // Breakpoint PCs are normalized when the trap is classified. With the
            // original instruction restored there is no repair step left to run.
            thread.stopped_at_breakpoint = None;
        }
    }
    Ok(())
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
        ptrace.remove_breakpoint(inferior.tgid, &mut inferior.breakpoints, address)?;
        let removed = inferior.breakpoints.remove(&address);
        assert!(removed.is_some(), "empty breakpoint site existed");
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
