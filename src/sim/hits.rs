//! What the client asks its breakpoints to do at their hits, and the check
//! that the debugger did it.
//!
//! Every hit counts. A hit stops when its breakpoint's hit condition
//! accepts the hit's number, its condition holds, or fails to evaluate,
//! which stops too, and it logs no message; a breakpoint that logs logs
//! instead of stopping. The client knows whether a condition holds at every
//! hit only where it can tell without the debugger: a constant, or a
//! source marker's condition, or its negation, at the start of the
//! marker's line in unoptimized code. Elsewhere it accepts either outcome.

use std::collections::{BTreeMap, BTreeSet};

use crate::{HitCondition, ProcessId, StateSnapshot, StopReason, ThreadState};

/// What the client knows of a breakpoint's condition at every hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    /// The breakpoint has no condition.
    Absent,
    Holds,
    Fails,
    /// The debugger alone can tell, or the condition may not evaluate.
    Unknown,
}

/// What a breakpoint was asked to do at its hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub hit_condition: Option<HitCondition>,
    pub condition: Known,
    pub logs: bool,
}

impl Policy {
    const fn holds(self) -> bool {
        matches!(self.condition, Known::Absent | Known::Holds)
    }

    fn allows(self, hit: u64) -> bool {
        self.hit_condition
            .is_none_or(|condition| condition.is_met(hit))
    }

    pub(super) fn must_stop(self, hit: u64) -> bool {
        self.allows(hit) && self.holds() && !self.logs
    }

    /// A condition that fails to evaluate stops even a breakpoint that logs.
    pub(super) fn may_stop(self, hit: u64) -> bool {
        self.allows(hit)
            && match self.condition {
                Known::Absent | Known::Holds => !self.logs,
                Known::Fails => false,
                Known::Unknown => true,
            }
    }

    /// Whether every hit must stop, whatever its number.
    pub(super) const fn unconditional(self) -> bool {
        self.hit_condition.is_none() && self.holds() && !self.logs
    }

    fn must_log(self, hit: u64) -> bool {
        self.allows(hit) && self.holds() && self.logs
    }

    fn may_log(self, hit: u64) -> bool {
        self.allows(hit) && self.condition != Known::Fails && self.logs
    }
}

/// Per breakpoint, the events the debugger published about its hits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    pub logged: BTreeMap<u64, u64>,
    pub condition_failures: BTreeMap<u64, u64>,
    /// How many times the reader of these events fell behind, after which
    /// the counts are incomplete.
    pub gaps: u64,
}

/// What the client saw at the last stop of a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub process: ProcessId,
    pub counts: BTreeMap<u64, u64>,
    pub published: Published,
}

/// What the check found worth counting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Found {
    /// A hit counted that did not stop.
    pub declined: bool,
    /// A stop at a hit whose condition the client knew held.
    pub held: bool,
    /// A hit logged a message.
    pub logged: bool,
}

/// The hits each breakpoint stopped threads at, by breakpoint.
fn stopping_hits(snapshot: &StateSnapshot) -> BTreeMap<u64, BTreeSet<u64>> {
    let mut stopping = BTreeMap::<u64, BTreeSet<u64>>::new();
    for thread in snapshot.threads.iter() {
        if let ThreadState::Stopped {
            reason: Some(StopReason::Breakpoint { hits, .. }),
        } = &thread.state
        {
            for hit in hits.iter() {
                stopping
                    .entry(hit.breakpoint.get())
                    .or_default()
                    .insert(hit.hit_count);
            }
        }
    }
    stopping
}

/// Judges the hits counted between the last stop of the same process and
/// this one, for each breakpoint the client keeps, with every policy it
/// had since then: a change while the program ran applies from some hit
/// the client cannot know.
pub fn judge(
    baseline: Option<&Baseline>,
    process: ProcessId,
    snapshot: &StateSnapshot,
    policies: &BTreeMap<u64, Vec<Policy>>,
    published: &Published,
) -> Result<Found, String> {
    let baseline = baseline.filter(|baseline| baseline.process == process);
    let stopping = stopping_hits(snapshot);
    let complete = baseline.is_some_and(|baseline| baseline.published.gaps == published.gaps);
    let since = |counts: &BTreeMap<u64, u64>, id: u64, before: Option<&BTreeMap<u64, u64>>| {
        counts.get(&id).copied().unwrap_or(0)
            - before.and_then(|b| b.get(&id)).copied().unwrap_or(0)
    };
    let mut found = Found::default();
    for breakpoint in snapshot.breakpoints.iter() {
        let id = breakpoint.id.get();
        let Some(versions) = policies.get(&id).filter(|versions| !versions.is_empty()) else {
            continue;
        };
        let before = baseline
            .and_then(|baseline| baseline.counts.get(&id))
            .copied()
            .unwrap_or(0);
        let now = breakpoint.hit_count;
        if now < before {
            return Err(format!(
                "breakpoint {id}'s hit count went down from {before} to {now}"
            ));
        }
        // A thread not resumed since an earlier stop keeps that stop's
        // reason, whose hits were judged then.
        let stopped = stopping
            .get(&id)
            .map(|hits| hits.range(before + 1..).copied().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        for &hit in &stopped {
            if hit > now {
                return Err(format!(
                    "breakpoint {id} stopped at hit {hit}, but counts {now} hits"
                ));
            }
            if !versions.iter().any(|policy| policy.may_stop(hit)) {
                return Err(format!(
                    "breakpoint {id} stopped at hit {hit}, which none of {versions:?} lets stop"
                ));
            }
            found.held |= versions
                .iter()
                .all(|policy| policy.condition == Known::Holds);
        }
        for hit in before + 1..=now {
            if stopped.contains(&hit) {
                continue;
            }
            found.declined = true;
            if versions.iter().all(|policy| policy.must_stop(hit)) {
                return Err(format!(
                    "hit {hit} of breakpoint {id} had to stop under {versions:?}, but did not"
                ));
            }
        }
        if !complete {
            continue;
        }
        let before_published = baseline.map(|baseline| &baseline.published);
        let logged = since(
            &published.logged,
            id,
            before_published.map(|published| &published.logged),
        );
        let must = (before + 1..=now)
            .filter(|&hit| versions.iter().all(|policy| policy.must_log(hit)))
            .count() as u64;
        let may = (before + 1..=now)
            .filter(|&hit| versions.iter().any(|policy| policy.may_log(hit)))
            .count() as u64;
        if !(must..=may).contains(&logged) {
            return Err(format!(
                "breakpoint {id} logged {logged} messages for hits {}..={now}, which must log \
                 {must} and may log {may} under {versions:?}",
                before + 1
            ));
        }
        found.logged |= logged > 0;
        let failed = since(
            &published.condition_failures,
            id,
            before_published.map(|published| &published.condition_failures),
        );
        if failed > 0
            && versions
                .iter()
                .all(|policy| policy.condition != Known::Unknown)
        {
            return Err(format!(
                "breakpoint {id}'s condition failed to evaluate {failed} times, though its \
                 value was known under {versions:?}"
            ));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        Breakpoint, BreakpointHit, BreakpointId, BreakpointSpec, HitComparison, InferiorState,
        ThreadId, ThreadSnapshot, VirtualAddress,
    };

    const PROCESS: ProcessId = ProcessId::new(1000);

    /// A stop at which breakpoint 1 has counted `count` hits and thread
    /// 1000 stopped at its hits `stopped`.
    fn stop(count: u64, stopped: &[u64]) -> StateSnapshot {
        let hits = stopped
            .iter()
            .map(|&hit_count| BreakpointHit {
                breakpoint: BreakpointId::new(1),
                hit_count,
            })
            .collect::<Arc<[_]>>();
        StateSnapshot {
            revision: 1,
            inferior: InferiorState::NotRunning,
            stop_id: None,
            selected: None,
            selected_frame: None,
            threads: Arc::from([ThreadSnapshot {
                id: ThreadId::new(1000),
                state: ThreadState::Stopped {
                    reason: (!hits.is_empty()).then(|| StopReason::Breakpoint {
                        address: VirtualAddress::new(0x1000),
                        hits,
                    }),
                },
                name: None,
            }]),
            presentation: None,
            breakpoints: Arc::from([Breakpoint {
                id: BreakpointId::new(1),
                spec: BreakpointSpec::Function("f".into()),
                locations: Arc::from([]),
                hit_condition: None,
                condition: None,
                log_message: None,
                hit_count: count,
            }]),
            watchpoints: Arc::from([]),
        }
    }

    fn baseline(count: u64, logged: u64) -> Baseline {
        Baseline {
            process: PROCESS,
            counts: BTreeMap::from([(1, count)]),
            published: Published {
                logged: BTreeMap::from([(1, logged)]),
                ..Published::default()
            },
        }
    }

    fn published(logged: u64) -> Published {
        Published {
            logged: BTreeMap::from([(1, logged)]),
            ..Published::default()
        }
    }

    fn policy(hit_condition: Option<HitCondition>, condition: Known, logs: bool) -> Policy {
        Policy {
            hit_condition,
            condition,
            logs,
        }
    }

    /// Hits stop exactly where their policies say they must or may; a
    /// policy changed between stops allows what any version allowed and
    /// requires what every version required; and hits that log publish one
    /// message each.
    #[test]
    fn hits_stop_and_log_as_their_policies_say() {
        let at_least_three = HitCondition::new(HitComparison::GreaterOrEqual, 3).ok();
        let judge_with = |versions: Vec<Policy>, before: Baseline, now: StateSnapshot, logs| {
            judge(
                Some(&before),
                PROCESS,
                &now,
                &BTreeMap::from([(1, versions)]),
                &published(logs),
            )
        };
        let counted = policy(at_least_three, Known::Absent, false);
        // Hits 2 and 3 counted; 3 stopped, as it must.
        assert!(judge_with(vec![counted], baseline(1, 0), stop(3, &[3]), 0).is_ok());
        // Hit 3 had to stop.
        assert!(judge_with(vec![counted], baseline(1, 0), stop(3, &[]), 0).is_err());
        // Hit 2 may not stop.
        assert!(judge_with(vec![counted], baseline(1, 0), stop(3, &[2, 3]), 0).is_err());
        // A stop judged at an earlier stop is not judged again.
        assert!(judge_with(vec![counted], baseline(3, 0), stop(3, &[3]), 0).is_ok());
        // A condition known to fail never stops; an unknown one may.
        let never = policy(None, Known::Fails, false);
        assert!(judge_with(vec![never], baseline(0, 0), stop(1, &[1]), 0).is_err());
        let unknown = policy(None, Known::Unknown, false);
        assert!(judge_with(vec![unknown], baseline(0, 0), stop(1, &[]), 0).is_ok());
        // Changed while running from failing to stopping: hit 1 may have
        // stopped under either.
        let always = policy(None, Known::Holds, false);
        assert!(judge_with(vec![never, always], baseline(0, 0), stop(1, &[1]), 0).is_ok());
        assert!(judge_with(vec![never, always], baseline(0, 0), stop(1, &[]), 0).is_ok());
        // A breakpoint that logs logs instead of stopping, once per hit.
        let logging = policy(None, Known::Holds, true);
        assert!(judge_with(vec![logging], baseline(0, 4), stop(2, &[]), 6).is_ok());
        assert!(judge_with(vec![logging], baseline(0, 4), stop(2, &[]), 5).is_err());
        assert!(judge_with(vec![logging], baseline(0, 4), stop(2, &[2]), 6).is_err());
        // A count never goes down within a process.
        assert!(judge_with(vec![always], baseline(2, 0), stop(1, &[]), 0).is_err());
    }
}
