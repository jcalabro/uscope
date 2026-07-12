use std::collections::BTreeMap;

use crate::{Error, ExecutionId, Result, StopId, StopReason, ThreadId};

/// Identifies one internal all-stop coordination barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarrierId(u64);

/// The execution state needed to coordinate one native thread.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ThreadControlState {
    Starting,
    Running {
        execution: ExecutionId,
    },
    StopRequested {
        execution: ExecutionId,
        barrier: BarrierId,
    },
    Stopped {
        reason: Option<StopReason>,
    },
    Exiting,
}

#[derive(Debug)]
struct StopBarrier {
    execution: ExecutionId,
    triggering_thread: ThreadId,
    reason: StopReason,
}

/// Coordinates thread state and all-stop barrier completion without platform details.
#[derive(Debug, Default)]
struct ProcessControl {
    threads: BTreeMap<ThreadId, ThreadControlState>,
    barrier: Option<StopBarrier>,
    current_stop: Option<StopId>,
    next_stop: u64,
    next_barrier: u64,
}

/// A completed, externally observable all-stop snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletedStop {
    pub stop_id: StopId,
    pub execution: ExecutionId,
    pub triggering_thread: ThreadId,
    pub reason: StopReason,
}

impl ProcessControl {
    pub fn insert(&mut self, thread: ThreadId, state: ThreadControlState) {
        assert!(
            self.threads.insert(thread, state).is_none(),
            "unique thread"
        );
    }

    pub fn begin_visible_stop(
        &mut self,
        execution: ExecutionId,
        triggering_thread: ThreadId,
        reason: StopReason,
    ) -> Vec<ThreadId> {
        assert!(
            self.barrier.is_none(),
            "only one stop barrier may be active"
        );

        let trigger = self
            .threads
            .get_mut(&triggering_thread)
            .expect("triggering thread is known");
        assert!(
            matches!(trigger, ThreadControlState::Running { .. }),
            "triggering thread was running"
        );
        *trigger = ThreadControlState::Stopped {
            reason: Some(reason.clone()),
        };

        self.next_barrier = self.next_barrier.wrapping_add(1);
        let barrier = BarrierId(self.next_barrier);
        let mut requested = Vec::new();

        for (&thread, state) in &mut self.threads {
            if matches!(state, ThreadControlState::Running { .. }) {
                *state = ThreadControlState::StopRequested { execution, barrier };
                requested.push(thread);
            }
        }

        self.barrier = Some(StopBarrier {
            execution,
            triggering_thread,
            reason,
        });
        requested
    }

    pub fn record_stop(&mut self, thread: ThreadId, reason: Option<StopReason>) {
        let state = self
            .threads
            .get_mut(&thread)
            .expect("stopped thread is known");
        assert!(
            matches!(
                state,
                ThreadControlState::Running { .. }
                    | ThreadControlState::StopRequested { .. }
                    | ThreadControlState::Starting
            ),
            "only an executing thread can report a new stop"
        );
        *state = ThreadControlState::Stopped { reason };
    }

    pub fn barrier_complete(&self) -> bool {
        self.barrier.is_some()
            && self
                .threads
                .values()
                .all(|state| matches!(state, ThreadControlState::Stopped { .. }))
    }

    pub fn complete_barrier(&mut self) -> CompletedStop {
        assert!(self.barrier_complete(), "all live threads are stopped");
        let barrier = self.barrier.take().expect("stop barrier exists");

        self.next_stop = self.next_stop.wrapping_add(1);
        let stop_id = StopId::new(self.next_stop);
        self.current_stop = Some(stop_id);

        CompletedStop {
            stop_id,
            execution: barrier.execution,
            triggering_thread: barrier.triggering_thread,
            reason: barrier.reason,
        }
    }

    pub fn validate_stop(&self, stop_id: StopId) -> Result<()> {
        if self.current_stop == Some(stop_id) {
            Ok(())
        } else {
            Err(Error::StaleStop)
        }
    }

    pub fn clear_stop(&mut self) {
        self.current_stop = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StepKind, VirtualAddress};

    #[test]
    fn visible_stop_completes_only_after_every_live_thread_stops() {
        let mut process = ProcessControl::default();
        let execution = ExecutionId::new(4);
        let first = ThreadId::new(11);
        let second = ThreadId::new(12);
        let third = ThreadId::new(13);

        for thread in [first, second] {
            process.insert(thread, ThreadControlState::Running { execution });
        }
        process.insert(third, ThreadControlState::Starting);

        let requested = process.begin_visible_stop(
            execution,
            second,
            StopReason::Breakpoint {
                address: VirtualAddress::new(0x1234),
            },
        );

        assert_eq!(requested, [first]);
        assert!(!process.barrier_complete());

        process.record_stop(third, None);
        assert!(!process.barrier_complete());

        process.record_stop(
            first,
            Some(StopReason::Step {
                kind: StepKind::Instruction,
            }),
        );
        assert!(process.barrier_complete());

        let completed = process.complete_barrier();
        assert_eq!(completed.stop_id, StopId::new(1));
        assert_eq!(completed.triggering_thread, second);
        assert!(matches!(completed.reason, StopReason::Breakpoint { .. }));
        assert!(process.validate_stop(completed.stop_id).is_ok());

        process.clear_stop();
        assert!(matches!(
            process.validate_stop(completed.stop_id),
            Err(Error::StaleStop)
        ));
    }

    #[test]
    fn exit_during_barrier_completes_only_after_the_thread_is_removed() {
        let mut process = ProcessControl::default();
        let execution = ExecutionId::new(8);
        let trigger = ThreadId::new(21);
        let sibling = ThreadId::new(22);

        process.insert(trigger, ThreadControlState::Running { execution });
        process.insert(sibling, ThreadControlState::Running { execution });
        process.begin_visible_stop(execution, trigger, StopReason::Pause);

        assert!(!process.barrier_complete());
        *process.threads.get_mut(&sibling).expect("known sibling") = ThreadControlState::Exiting;
        assert!(!process.barrier_complete());
        process.threads.remove(&sibling);
        assert!(process.barrier_complete());
    }
}
