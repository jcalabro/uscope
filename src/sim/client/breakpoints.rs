//! The user's breakpoints and watchpoints: adding, amending, and removing
//! them, with the conditions the client knows the outcome of, and judging
//! the hits counted at each stop.

use std::collections::BTreeMap;

use super::{Client, protocol};
use crate::sim::choices::Stream;
use crate::sim::hits::{self, Baseline, Known, Policy};
use crate::sim::kernel::DebugBehavior;
use crate::sim::markers::Marker;
use crate::sim::marks::Mark;
use crate::sim::report::Failure;
use crate::sim::watches::{self, Intent};
use crate::{
    BreakpointId, BreakpointLocation, BreakpointOptions, BreakpointSpec, Condition, Error,
    HitComparison, HitCondition, LineNumber, LogMessage, ProcessId, StateSnapshot, VirtualAddress,
    WatchAccess, Watchpoint, WatchpointId, WatchpointOptions, WatchpointSpec,
};

/// A breakpoint the client added, with the image addresses of its traps.
pub(super) struct Added {
    id: BreakpointId,
    pub(super) traps: Vec<u64>,
    /// Whether the debugger last said it is enabled.
    enabled: bool,
    temporary: bool,
    /// The marker line it was set at, if any.
    marker_line: Option<u64>,
    /// What it was asked to do at its hits since the last stop, oldest
    /// first: a change while the program runs applies from a hit the client
    /// cannot know.
    policies: Vec<Policy>,
}

impl Added {
    pub(super) const fn id(&self) -> BreakpointId {
        self.id
    }
}

impl Client {
    /// Watches one of the program's small objects, by name or by address,
    /// for one kind of access. The kernel's debug registers may refuse.
    pub(super) async fn add_watch(&self) -> Result<(), Failure> {
        if self.script.globals.is_empty() {
            return Ok(());
        }
        let (name, image_address, size) = self
            .choices
            .borrow_mut()
            .pick(Stream::Client, &self.script.globals)
            .clone();
        // Watches on changes, filtered at each trap and judged again at
        // each stop, are the most intricate.
        let accesses = [
            WatchAccess::Change,
            WatchAccess::Change,
            WatchAccess::Write,
            WatchAccess::ReadWrite,
            WatchAccess::Read,
        ];
        let access = *self.choices.borrow_mut().pick(Stream::Client, &accesses);
        // Four slots watch at most 32 bytes; a larger object is watched in
        // part, anywhere inside it.
        let whole = size <= 32;
        let (name, image_address, size) = if whole {
            (name, image_address, size)
        } else {
            let width = 1 + self.draw(16);
            let offset = self.draw(size - width + 1);
            (format!("{name}+{offset}"), image_address + offset, width)
        };
        let (options, policy) = self.choose_watch_options();
        let result = if whole && self.draw(2) == 0 {
            match crate::Expression::name(&name) {
                Some(expression) => self.handle.watch_with(&expression, access, options).await,
                None => {
                    return Err(Failure::simulator(
                        "client",
                        format!("`{name}` has no expression"),
                    ));
                }
            }
        } else {
            let address = VirtualAddress::new(image_address + self.main_bias().await?);
            self.handle
                .add_watchpoint_with(
                    WatchpointSpec::Location {
                        address,
                        byte_size: size,
                    },
                    access,
                    options,
                )
                .await
        };
        match result {
            Ok(watchpoint) => self.armed(&watchpoint, &name, policy),
            Err(Error::UnsupportedWatchAccess(WatchAccess::Read))
                if access == WatchAccess::Read =>
            {
                self.note("watching loads alone refused");
            }
            Err(Error::WatchpointCapacity { .. }) => self.note(format!("no slots for {name}")),
            Err(Error::HardwareWatchpointsUnavailable(description))
                if self.script.debug == DebugBehavior::Discarding =>
            {
                self.note(format!("watching refused: {description}"));
                self.mark(Mark::WatchRefused);
            }
            Err(Error::WatchpointHardwareBusy { thread })
                if matches!(self.script.debug, DebugBehavior::Contended(_)) =>
            {
                self.note(format!("thread {thread}'s slots are busy"));
                self.mark(Mark::WatchRefused);
            }
            Err(error) => {
                return Err(protocol(format!(
                    "watching {name} for {access} failed: {error}"
                )));
            }
        }
        Ok(())
    }

    /// Notes a watchpoint the debugger armed, which the kernel then
    /// follows.
    fn armed(&self, watchpoint: &Watchpoint, name: &str, policy: Policy) {
        self.note(format!(
            "watchpoint {} on {name} for {}: {:x?} {:?} {:?}",
            watchpoint.id,
            watchpoint.access,
            watchpoint.coverage,
            watchpoint.hit_condition,
            watchpoint.condition
        ));
        self.shared.watches.borrow_mut().insert(
            watchpoint.id.get(),
            Intent {
                id: watchpoint.id.get(),
                address: watchpoint.address.get(),
                size: watchpoint.byte_size,
                access: watchpoint.access,
                spans: watchpoint
                    .coverage
                    .iter()
                    .map(|span| (span.start.get(), span.end.get()))
                    .collect(),
                policies: vec![policy],
            },
        );
        self.mark(Mark::WatchAdded);
    }

    /// Chooses a new watchpoint's options: half none, the rest a hit
    /// condition, a condition whose value the client knows, or both.
    fn choose_watch_options(&self) -> (WatchpointOptions, Policy) {
        if self.draw(2) == 0 {
            return (WatchpointOptions::default(), UNCONDITIONAL);
        }
        let condition = self.choose_condition(None);
        let options = WatchpointOptions {
            hit_condition: self.choose_hit_condition(),
            condition: condition.parse(&self.script.markers, None),
            ..WatchpointOptions::default()
        };
        let policy = Policy {
            hit_condition: options.hit_condition,
            condition: self.known(condition, None, &[]),
            logs: false,
            enabled: true,
        };
        (options, policy)
    }

    /// Changes one watchpoint's hit condition or condition, which applies
    /// from the next hit the debugger handles.
    pub(super) async fn amend_watch(&self) -> Result<(), Failure> {
        let ids = self
            .shared
            .watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let id = *self.choices.borrow_mut().pick(Stream::Client, &ids);
        let mut policy = *self.shared.watches.borrow()[&id]
            .policies
            .last()
            .expect("a watch has a policy");
        let watchpoint = WatchpointId::new(id);
        let request = if self.draw(2) == 0 {
            let hit_condition = self.choose_hit_condition();
            policy.hit_condition = hit_condition;
            Ok(hit_condition)
        } else {
            let choice = self.choose_condition(None);
            policy.condition = self.known(choice, None, &[]);
            Err(choice.parse(&self.script.markers, None))
        };
        // From the moment the client asks, the new policy may apply.
        if let Some(intent) = self.shared.watches.borrow_mut().get_mut(&id) {
            intent.policies.push(policy);
        }
        let amended = match request {
            Ok(hit_condition) => {
                self.handle
                    .set_watchpoint_hit_condition(watchpoint, hit_condition)
                    .await
            }
            Err(condition) => {
                self.handle
                    .set_watchpoint_condition(watchpoint, condition)
                    .await
            }
        }
        .map_err(|error| protocol(format!("amending watchpoint {id} failed: {error}")))?;
        self.note(format!(
            "watchpoint {id} now {:?} {:?}",
            amended.hit_condition, amended.condition
        ));
        self.mark(Mark::WatchAmended);
        Ok(())
    }

    /// Disables a watchpoint, or, at a stop, enables a disabled one, which
    /// the debug registers may then refuse. A watch enabled while the
    /// program ran would take bytes the oracles cannot know as its last
    /// observed ones, so only a stop enables.
    pub(super) async fn toggle_watch(&self, running: bool) -> Result<(), Failure> {
        let disabled = self
            .disabled_watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if !running && !disabled.is_empty() && self.control(2) == 0 {
            let id = *self.choices.borrow_mut().pick(Stream::Control, &disabled);
            return self.enable_watch(id).await;
        }
        let ids = self
            .shared
            .watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let id = *self.choices.borrow_mut().pick(Stream::Control, &ids);
        // From the moment the client asks, the watchpoint may report no
        // more.
        let intent = self
            .shared
            .watches
            .borrow_mut()
            .remove(&id)
            .expect("a known watch");
        let watchpoint = match self
            .handle
            .set_watchpoint_enabled(WatchpointId::new(id), false)
            .await
        {
            Ok(watchpoint) => watchpoint,
            Err(error) => {
                // A failed edit leaves the watchpoint as it was.
                self.shared.watches.borrow_mut().insert(id, intent);
                return Err(protocol(format!(
                    "disabling watchpoint {id} failed: {error}"
                )));
            }
        };
        if watchpoint.enabled {
            return Err(protocol(format!(
                "disabling watchpoint {id} returned {watchpoint:?}"
            )));
        }
        self.disabled_watches
            .borrow_mut()
            .insert(id, (intent, watchpoint.hit_count));
        if running {
            self.released_while_running.set(true);
        }
        self.note(format!("disabled watchpoint {id}"));
        self.mark(Mark::WatchDisabled);
        Ok(())
    }

    async fn enable_watch(&self, id: u64) -> Result<(), Failure> {
        match self
            .handle
            .set_watchpoint_enabled(WatchpointId::new(id), true)
            .await
        {
            Ok(watchpoint) if watchpoint.enabled => {
                let (mut intent, _) = self
                    .disabled_watches
                    .borrow_mut()
                    .remove(&id)
                    .expect("a disabled watch");
                intent.policies.drain(..intent.policies.len() - 1);
                self.shared.watches.borrow_mut().insert(id, intent);
                self.note(format!("enabled watchpoint {id}"));
                self.mark(Mark::WatchEnabled);
            }
            Err(Error::WatchpointCapacity { .. }) => {
                self.note(format!("no slots to enable watchpoint {id}"));
            }
            Err(Error::HardwareWatchpointsUnavailable(description))
                if self.script.debug == DebugBehavior::Discarding =>
            {
                self.note(format!("enabling refused: {description}"));
            }
            Err(Error::WatchpointHardwareBusy { thread })
                if matches!(self.script.debug, DebugBehavior::Contended(_)) =>
            {
                self.note(format!("thread {thread}'s slots are busy"));
            }
            other => {
                return Err(protocol(format!(
                    "enabling watchpoint {id} returned {other:?}"
                )));
            }
        }
        Ok(())
    }

    /// Watch accounting for disabled watchpoints, at each stop. One whose
    /// storage ended is gone.
    fn judge_disabled_watches(&self, snapshot: &StateSnapshot) -> Result<(), Failure> {
        let mut disabled = self.disabled_watches.borrow_mut();
        disabled.retain(|id, _| {
            snapshot
                .watchpoints
                .iter()
                .any(|watchpoint| watchpoint.id.get() == *id)
        });
        let counted = disabled
            .iter()
            .map(|(&id, (_, counted))| (id, *counted))
            .collect();
        watches::judge_disabled(&counted, snapshot)
            .map_err(|message| Failure::debugger("watch accounting", message))
    }

    /// Removes one watchpoint, if any.
    pub(super) async fn remove_watch(&self) -> Result<(), Failure> {
        let ids = self
            .shared
            .watches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let id = *self.choices.borrow_mut().pick(Stream::Client, &ids);
        // From the moment the client asks, the watchpoint may be gone.
        self.shared.watches.borrow_mut().remove(&id);
        let id = crate::WatchpointId::new(id);
        self.handle
            .remove_watchpoint(id)
            .await
            .map_err(|error| protocol(format!("removing watchpoint {id} failed: {error}")))?;
        self.note(format!("removed watchpoint {id}"));
        Ok(())
    }

    /// A location for a breakpoint or an advance: a function, or a line,
    /// half of them a marker's.
    pub(super) fn choose_spec(&self, stream: Stream) -> BreakpointSpec {
        let draw = |bound| self.choices.borrow_mut().below(stream, bound);
        if draw(2) == 0 {
            let function = self
                .choices
                .borrow_mut()
                .pick(stream, &self.script.functions)
                .clone();
            BreakpointSpec::Function(function)
        } else {
            // Half the lines aim at markers, where the variables oracle
            // judges what the debugger shows.
            let marker_lines = self.script.markers.keys().copied().collect::<Vec<_>>();
            let line = if !marker_lines.is_empty() && draw(2) == 0 {
                *self.choices.borrow_mut().pick(stream, &marker_lines)
            } else {
                draw(self.script.source_lines) + 1
            };
            BreakpointSpec::Source {
                path: self.script.source.clone(),
                line: LineNumber::new(line).expect("lines count from one"),
            }
        }
    }

    /// Adds a breakpoint, returning whether the debugger made one.
    pub(super) async fn add_breakpoint(
        &self,
        breakpoints: &mut Vec<Added>,
    ) -> Result<bool, Failure> {
        let spec = self.choose_spec(Stream::Client);
        let marker_line = match &spec {
            BreakpointSpec::Source { line, .. } => {
                Some(line.get()).filter(|line| self.script.markers.contains_key(line))
            }
            _ => None,
        };
        let (options, condition) = self.choose_options(marker_line);
        let breakpoint = match self.handle.add_breakpoint_with(spec.clone(), options).await {
            Ok(breakpoint) => breakpoint,
            Err(error) if self.names_no_code(&spec, &error) => return Ok(false),
            Err(error) => {
                return Err(protocol(format!(
                    "adding a breakpoint at {spec} failed: {error}"
                )));
            }
        };
        self.note(format!(
            "breakpoint {} at {spec}: {} locations, {:?} {:?}{}",
            breakpoint.id,
            breakpoint.locations.len(),
            breakpoint.hit_condition,
            breakpoint.condition,
            if breakpoint.log_message.is_some() {
                ", logging"
            } else {
                ""
            }
        ));
        let traps = image_traps(&breakpoint);
        // A temporary breakpoint may be gone at any stop, so only the hit
        // oracle follows it.
        if !breakpoint.enabled {
            self.shared
                .disabled
                .borrow_mut()
                .insert(breakpoint.id.get());
        } else if !breakpoint.temporary {
            self.shared
                .breakpoints
                .borrow_mut()
                .insert(breakpoint.id.get(), traps.iter().copied().collect());
        }
        let policy = Policy {
            hit_condition: breakpoint.hit_condition,
            condition: self.known(condition, marker_line, &traps),
            logs: breakpoint.log_message.is_some(),
            enabled: breakpoint.enabled,
        };
        // Adding what a breakpoint already is returns it, with the hits it
        // counted under what it was asked before.
        if let Some(existing) = breakpoints
            .iter_mut()
            .find(|added| added.id == breakpoint.id)
        {
            existing.policies.push(policy);
        } else {
            breakpoints.push(Added {
                id: breakpoint.id,
                traps,
                enabled: breakpoint.enabled,
                temporary: breakpoint.temporary,
                marker_line,
                policies: vec![policy],
            });
        }
        if breakpoint.temporary {
            self.mark(Mark::TemporaryAdded);
        }
        Ok(true)
    }

    /// Whether `error` refuses `spec` because it names no code in this
    /// variant, which the client notes.
    pub(super) fn names_no_code(&self, spec: &BreakpointSpec, error: &Error) -> bool {
        match error {
            // A line outside every function names no code.
            Error::SourceLineUnavailable { line, .. } if matches!(spec, BreakpointSpec::Source { line: wanted, .. } if wanted.get() == *line) =>
            {
                self.note(format!("{spec} has no code"));
                true
            }
            // Another variant may define a function this one inlined away.
            Error::FunctionNotFound(name) | Error::SymbolNotFound(name)
                if matches!(spec, BreakpointSpec::Function(wanted) if wanted == name)
                    && !self.script.defined.contains(name) =>
            {
                self.note(format!("{name} is not in this variant"));
                true
            }
            _ => false,
        }
    }

    /// Disables an enabled breakpoint or enables a disabled one, which the
    /// debugger resolves again.
    pub(super) async fn toggle_breakpoint(
        &self,
        breakpoints: &mut [Added],
    ) -> Result<bool, Failure> {
        if breakpoints.is_empty() {
            return Ok(false);
        }
        let index = usize::try_from(self.control(breakpoints.len() as u64)).expect("small");
        let added = &mut breakpoints[index];
        let (id, enabled) = (added.id, !added.enabled);
        // From the moment the client asks, either state may apply.
        if enabled {
            self.shared.disabled.borrow_mut().remove(&id.get());
        } else {
            self.shared.breakpoints.borrow_mut().remove(&id.get());
        }
        let mut policy = *added.policies.last().expect("a breakpoint has a policy");
        policy.enabled = enabled;
        added.policies.push(policy);
        let breakpoint = match self.handle.set_breakpoint_enabled(id, enabled).await {
            Ok(breakpoint) => breakpoint,
            Err(error) if self.deleted_by_stop(added.temporary, &error) => return Ok(false),
            Err(error) => {
                // A failed edit leaves the breakpoint as it was, as when
                // the process ends during it.
                policy.enabled = !enabled;
                added.policies.push(policy);
                if enabled {
                    self.shared.disabled.borrow_mut().insert(id.get());
                } else if !added.temporary {
                    self.shared
                        .breakpoints
                        .borrow_mut()
                        .insert(id.get(), added.traps.iter().copied().collect());
                }
                return Err(protocol(format!(
                    "{} breakpoint {id} failed: {error}",
                    if enabled { "enabling" } else { "disabling" }
                )));
            }
        };
        if breakpoint.enabled != enabled {
            return Err(protocol(format!(
                "setting breakpoint {id} enabled {enabled} returned {breakpoint:?}"
            )));
        }
        added.enabled = enabled;
        if enabled {
            added.traps = image_traps(&breakpoint);
            if !added.temporary {
                self.shared
                    .breakpoints
                    .borrow_mut()
                    .insert(id.get(), added.traps.iter().copied().collect());
            }
            self.mark(Mark::BreakpointEnabled);
        } else {
            self.shared.disabled.borrow_mut().insert(id.get());
            self.mark(Mark::BreakpointDisabled);
        }
        self.note(format!(
            "breakpoint {id} {}",
            if enabled { "enabled" } else { "disabled" }
        ));
        Ok(true)
    }

    /// A hit condition for a new breakpoint, or none.
    fn choose_hit_condition(&self) -> Option<HitCondition> {
        if self.draw(2) == 0 {
            return None;
        }
        let comparisons = [
            HitComparison::Equal,
            HitComparison::NotEqual,
            HitComparison::Less,
            HitComparison::LessOrEqual,
            HitComparison::Greater,
            HitComparison::GreaterOrEqual,
            HitComparison::Multiple,
        ];
        let comparison = *self.choices.borrow_mut().pick(Stream::Client, &comparisons);
        HitCondition::new(comparison, self.draw(4) + 1).ok()
    }

    /// A condition for a breakpoint, a marker's only at its line.
    fn choose_condition(&self, marker_line: Option<u64>) -> ConditionChoice {
        let choices = if marker_line.is_some() {
            &[
                ConditionChoice::None,
                ConditionChoice::True,
                ConditionChoice::False,
                ConditionChoice::Marker,
                ConditionChoice::NotMarker,
            ][..]
        } else {
            &[
                ConditionChoice::None,
                ConditionChoice::True,
                ConditionChoice::False,
            ][..]
        };
        *self.choices.borrow_mut().pick(Stream::Client, choices)
    }

    /// What the client knows of `condition` at a breakpoint set at
    /// `marker_line` with traps at `traps`: a marker's condition holds at
    /// the start of its line in unoptimized code, where every variable has
    /// a value, and nowhere else is known.
    fn known(&self, condition: ConditionChoice, marker_line: Option<u64>, traps: &[u64]) -> Known {
        let at_marker = marker_line.is_some_and(|line| {
            !traps.is_empty()
                && traps
                    .iter()
                    .all(|trap| self.script.marker_rows.get(trap) == Some(&line))
        });
        match condition {
            ConditionChoice::None => Known::Absent,
            ConditionChoice::True => Known::Holds,
            ConditionChoice::False => Known::Fails,
            ConditionChoice::Marker if at_marker => Known::Holds,
            ConditionChoice::NotMarker if at_marker => Known::Fails,
            ConditionChoice::Marker | ConditionChoice::NotMarker => Known::Unknown,
        }
    }

    /// Chooses a new breakpoint's options: none, or a hit condition, a
    /// condition, and perhaps a message to log instead of stopping.
    fn choose_options(&self, marker_line: Option<u64>) -> (BreakpointOptions, ConditionChoice) {
        let (options, condition) = if self.draw(2) == 0 {
            (BreakpointOptions::default(), ConditionChoice::None)
        } else {
            let condition = self.choose_condition(marker_line);
            let options = BreakpointOptions {
                hit_condition: self.choose_hit_condition(),
                condition: condition.parse(&self.script.markers, marker_line),
                log_message: (self.draw(3) == 0)
                    .then(|| LogMessage::parse("hit").expect("a valid message")),
                ..BreakpointOptions::default()
            };
            (options, condition)
        };
        let options = BreakpointOptions {
            enabled: self.control(16) != 0,
            temporary: self.control(4) == 0,
            ..options
        };
        (options, condition)
    }

    /// Changes one breakpoint's hit condition or condition, which applies
    /// from the next hit the debugger handles.
    pub(super) async fn amend_breakpoint(&self, breakpoints: &mut [Added]) -> Result<(), Failure> {
        if breakpoints.is_empty() {
            return Ok(());
        }
        let index = usize::try_from(self.draw(breakpoints.len() as u64)).expect("small");
        let added = &mut breakpoints[index];
        let id = added.id;
        let mut policy = *added.policies.last().expect("a breakpoint has a policy");
        let amended = if self.draw(2) == 0 {
            let hit_condition = self.choose_hit_condition();
            policy.hit_condition = hit_condition;
            self.handle
                .set_breakpoint_hit_condition(id, hit_condition)
                .await
        } else {
            let choice = self.choose_condition(added.marker_line);
            policy.condition = self.known(choice, added.marker_line, &added.traps);
            self.handle
                .set_breakpoint_condition(id, choice.parse(&self.script.markers, added.marker_line))
                .await
        };
        let breakpoint = match amended {
            Ok(breakpoint) => breakpoint,
            Err(error) if self.deleted_by_stop(added.temporary, &error) => return Ok(()),
            Err(error) => {
                return Err(protocol(format!(
                    "amending breakpoint {id} failed: {error}"
                )));
            }
        };
        self.note(format!(
            "breakpoint {id} now {:?} {:?}",
            breakpoint.hit_condition, breakpoint.condition
        ));
        added.policies.push(policy);
        self.mark(Mark::BreakpointAmended);
        Ok(())
    }

    /// Whether `error` says a temporary breakpoint is gone, which a stop
    /// the client has yet to see may have deleted. The hit oracle judges
    /// that stop when the client sees it.
    fn deleted_by_stop(&self, temporary: bool, error: &Error) -> bool {
        let gone = temporary && matches!(error, Error::BreakpointNotFound(_));
        if gone {
            self.note(format!("{error}: a stop deleted it"));
        }
        gone
    }

    /// Judges the hits counted since the last stop of this process, and
    /// starts again from this one.
    pub(super) fn judge_hits(
        &self,
        process: ProcessId,
        snapshot: &StateSnapshot,
        breakpoints: &mut Vec<Added>,
    ) -> Result<(), Failure> {
        let ending = self.ending(process);
        let published = self.shared.published.borrow().clone();
        let policies = breakpoints
            .iter()
            .map(|added| {
                (
                    added.id.get(),
                    hits::Kept {
                        versions: added.policies.clone(),
                        temporary: added.temporary,
                    },
                )
            })
            .collect();
        // A process ending as a whole takes its threads out of their stops
        // whatever they hit, so its hits are not judged.
        if !ending {
            let found = hits::judge(
                self.baseline.borrow().as_ref(),
                process,
                snapshot,
                &policies,
                &published,
            )
            .map_err(|message| Failure::debugger("breakpoint conditions", message))?;
            if found.declined {
                self.mark(Mark::HitDeclined);
            }
            if found.held {
                self.mark(Mark::ConditionHeld);
            }
            if found.logged {
                self.mark(Mark::HitLogged);
            }
            if found.temporary_stopped {
                self.mark(Mark::TemporaryStop);
            }
            if found.temporary_shared {
                self.mark(Mark::TemporaryCoHit);
            }
            self.judge_disabled_watches(snapshot)?;
        }
        // A temporary breakpoint the stop deleted is the client's no more.
        let remaining = |added: &Added| {
            snapshot
                .breakpoints
                .iter()
                .any(|breakpoint| breakpoint.id == added.id)
        };
        breakpoints.retain(|added| !added.temporary || remaining(added));
        *self.baseline.borrow_mut() = Some(Baseline {
            process,
            counts: snapshot
                .breakpoints
                .iter()
                .map(|breakpoint| (breakpoint.id.get(), breakpoint.hit_count))
                .collect(),
            published,
        });
        for added in breakpoints.iter_mut() {
            let current = *added.policies.last().expect("a breakpoint has a policy");
            added.policies = vec![current];
        }
        Ok(())
    }

    /// Removes a breakpoint, returning whether there was one to remove.
    pub(super) async fn remove_breakpoint(
        &self,
        breakpoints: &mut Vec<Added>,
    ) -> Result<bool, Failure> {
        if breakpoints.is_empty() {
            return Ok(false);
        }
        let index = usize::try_from(self.draw(breakpoints.len() as u64)).expect("small");
        let removed = breakpoints.swap_remove(index);
        let id = removed.id;
        // From the moment the client asks, the breakpoint may be gone.
        self.shared.breakpoints.borrow_mut().remove(&id.get());
        self.shared.disabled.borrow_mut().remove(&id.get());
        match self.handle.remove_breakpoint(id).await {
            Ok(_) => {}
            Err(error) if self.deleted_by_stop(removed.temporary, &error) => return Ok(true),
            Err(error) => {
                return Err(protocol(format!(
                    "removing breakpoint {id} failed: {error}"
                )));
            }
        }
        self.note(format!("removed breakpoint {id}"));
        Ok(true)
    }

    /// Resumes from a stop where a new thread could not be armed, which
    /// the debugger must refuse: the slots others hold stay held.
    pub(super) async fn resume_unarmed(&self) -> Result<(), Failure> {
        match self.handle.resume().await {
            Err(error) if self.refused_unarmed(&error).await? => Ok(()),
            other => Err(protocol(format!(
                "resuming with a thread that cannot be armed returned {other:?}"
            ))),
        }
    }

    /// Whether `error` refuses to run a thread that slots others hold
    /// leave unarmed, which only contended debug registers explain. The
    /// client then frees a watchpoint, as a user would.
    pub(super) async fn refused_unarmed(&self, error: &Error) -> Result<bool, Failure> {
        let Error::WatchpointHardwareBusy { thread } = error else {
            return Ok(false);
        };
        if !matches!(self.script.debug, DebugBehavior::Contended(_)) {
            return Ok(false);
        }
        self.note(format!("running refused: thread {thread} cannot be armed"));
        self.mark(Mark::RunRefusedUnarmed);
        self.remove_watch().await?;
        Ok(true)
    }
}

/// What a watchpoint without a hit condition or condition does.
const UNCONDITIONAL: Policy = Policy {
    hit_condition: None,
    condition: Known::Absent,
    logs: false,
    enabled: true,
};

/// The image addresses of a breakpoint's locations in the program.
fn image_traps(breakpoint: &crate::Breakpoint) -> Vec<u64> {
    breakpoint
        .locations
        .iter()
        .filter_map(|location| match location.location {
            BreakpointLocation::Image(address) => Some(address.get()),
            BreakpointLocation::Virtual(_) => None,
        })
        .collect()
}

/// The condition a breakpoint or watchpoint is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionChoice {
    None,
    True,
    False,
    /// The condition of the marker at the breakpoint's line.
    Marker,
    /// Its negation.
    NotMarker,
}

impl ConditionChoice {
    fn parse(self, markers: &BTreeMap<u64, Marker>, line: Option<u64>) -> Option<Condition> {
        let marker = || {
            line.and_then(|line| markers.get(&line))
                .expect("marker conditions are chosen at marker lines")
        };
        let text = match self {
            Self::None => return None,
            Self::True => "true".to_owned(),
            Self::False => "false".to_owned(),
            Self::Marker => marker().text.clone(),
            Self::NotMarker => format!("!({})", marker().text),
        };
        Some(Condition::parse(&text).expect("the client's conditions parse"))
    }
}
