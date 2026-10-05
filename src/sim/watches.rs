//! Watch accounting: the watchpoint hits the debugger reports at a stop
//! against the accesses the CPU made to watched memory since the last.
//!
//! Every access a slot covers traps, so a thread that accessed a watched
//! range was stopped there. A watchpoint on stores or on any access reports
//! each such thread; one on changes reports the threads that stored once
//! the watched bytes differ from those at the last stop, and none when a
//! store left them as they were or another thread undid it.

use std::collections::{BTreeMap, BTreeSet};

use super::kernel::{Kernel, Tid};
use crate::{StopReason, WatchAccess};

/// A watchpoint the client was told exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub id: u64,
    pub address: u64,
    pub size: u64,
    pub access: WatchAccess,
    /// The hardware spans covering it, as start and end.
    pub spans: Vec<(u64, u64)>,
}

/// What the check found worth counting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Found {
    /// A reported hit.
    pub hit: bool,
    /// A hit reported for a thread other than the process's first.
    pub other_thread: bool,
    /// A store that left watched bytes as they were, which a watchpoint on
    /// stores reports and one on changes does not.
    pub unchanged: bool,
}

/// The bytes of `intent` in `tgid`'s memory now.
pub fn bytes(kernel: &Kernel, tgid: Tid, intent: &Intent) -> Option<Vec<u8>> {
    kernel
        .processes
        .get(&tgid)?
        .space
        .peek_bytes(intent.address, usize::try_from(intent.size).ok()?)
}

/// Judges the stop the controller just published for `tgid`.
pub fn judge(
    kernel: &Kernel,
    tgid: Tid,
    reasons: &BTreeMap<Tid, StopReason>,
    intents: &[Intent],
    baselines: &BTreeMap<u64, Vec<u8>>,
) -> Result<Found, String> {
    let mut found = Found::default();
    let accessors = |watch: u64| {
        kernel
            .watching
            .log
            .iter()
            .filter(|(_, watches)| watches.contains(&watch))
            .map(|(&tid, _)| tid)
            .collect::<BTreeSet<_>>()
    };
    for &tid in &kernel.watching.ran {
        if kernel
            .threads
            .get(&tid)
            .is_none_or(|thread| thread.tgid != tgid)
        {
            continue;
        }
        let accessed = kernel.watching.log.get(&tid).cloned().unwrap_or_default();
        let reported = match reasons.get(&tid) {
            Some(StopReason::Watchpoint { hits }) => hits.to_vec(),
            _ => Vec::new(),
        };
        for hit in &reported {
            let id = hit.watchpoint.get();
            let Some(intent) = intents.iter().find(|intent| intent.id == id) else {
                continue;
            };
            if !accessed.contains(&id) {
                return Err(format!(
                    "thread {tid} reported a hit on watchpoint {id}, but made no access to it \
                     since the last stop"
                ));
            }
            let now = bytes(kernel, tgid, intent);
            let before = baselines.get(&id).cloned();
            if hit.current.as_deref() != now.as_deref()
                || hit.previous.as_deref() != before.as_deref()
            {
                return Err(format!(
                    "thread {tid}'s hit on watchpoint {id} shows {:02x?} becoming {:02x?}; the \
                     bytes were {before:02x?} at the last stop and are {now:02x?}",
                    hit.previous, hit.current
                ));
            }
            if intent.access == WatchAccess::Change && now == before {
                return Err(format!(
                    "thread {tid} reported a change of watchpoint {id}, whose bytes are as \
                     they were at the last stop, {now:02x?}"
                ));
            }
            found.hit = true;
            found.other_thread |= tid != tgid;
        }
        for &id in &accessed {
            let Some(intent) = intents.iter().find(|intent| intent.id == id) else {
                continue;
            };
            let hit = reported.iter().any(|hit| hit.watchpoint.get() == id);
            let changed = bytes(kernel, tgid, intent) != baselines.get(&id).cloned();
            let required = match intent.access {
                WatchAccess::Write | WatchAccess::ReadWrite | WatchAccess::Read => true,
                // The thread that stored is the one to report only when no
                // other stored meanwhile.
                WatchAccess::Change => changed && accessors(id) == BTreeSet::from([tid]),
            };
            if required && !hit {
                return Err(format!(
                    "thread {tid} accessed watchpoint {id} ({}), but the stop reports no hit \
                     for it: {:?}",
                    intent.access,
                    reasons.get(&tid)
                ));
            }
            found.unchanged |= kernel.watching.unchanged.contains(&id);
        }
    }
    Ok(found)
}
