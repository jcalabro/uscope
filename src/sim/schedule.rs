//! Which action the world takes next.
//!
//! Every action belongs to an actor: a program thread, or a session's
//! waiter, controller, or client, or the follower that hands the children a
//! session holds to others. A seed's swarm picks one of two policies:
//!
//! - A random walk: an action kind by the swarm's weights, then an action
//!   of that kind uniformly.
//! - PCT (Burckhardt et al., 2010): every actor has a priority, and the
//!   enabled actor with the highest runs. At a few change points, chosen
//!   before the run, the actor that runs drops below every other. A bug that
//!   needs `d` orderings to happen is found with a probability that depends
//!   only on `d`, the number of actors, and the run's length.
//!
//! A thread that yields the CPU drops below every other actor under PCT
//! too, or a thread spinning on a lock would starve the thread holding it.

use std::collections::BTreeMap;
use std::fmt;

use super::choices::{Choices, Stream};
use super::kernel::Tid;
use super::swarm::Weights;

/// A session, by the order it started in: the first is the one the client
/// drives, and each later one adopts a child the first held.
pub type SessionId = usize;

/// One thing the world can do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A running thread executes a burst of instructions.
    Run(Tid),
    /// A session's waiter reaps a status and queues it for its controller.
    Collect(SessionId),
    /// A session's controller handles the message at the front of its
    /// queue.
    Deliver(SessionId),
    /// A session's client task runs until it waits again.
    Poll(SessionId),
    /// The next child the first session held is adopted by a session of
    /// its own, or released.
    Follow,
}

/// Who performs an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Actor {
    Thread(Tid),
    Waiter(SessionId),
    Controller(SessionId),
    Client(SessionId),
    Follower,
}

impl Action {
    const fn actor(self) -> Actor {
        match self {
            Self::Run(tid) => Actor::Thread(tid),
            Self::Collect(session) => Actor::Waiter(session),
            Self::Deliver(session) => Actor::Controller(session),
            Self::Poll(session) => Actor::Client(session),
            Self::Follow => Actor::Follower,
        }
    }

    /// The action's kind, which a random walk weighs: running, collecting,
    /// delivering, or acting for the user.
    const fn kind(self) -> usize {
        match self {
            Self::Run(_) => 0,
            Self::Collect(_) => 1,
            Self::Deliver(_) => 2,
            Self::Poll(_) | Self::Follow => 3,
        }
    }
}

/// How a run's actions are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Walk(Weights),
    /// PCT with `depth - 1` change points among the first `horizon`
    /// actions.
    Pct {
        depth: u64,
        horizon: u64,
    },
}

impl fmt::Display for Policy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Walk(Weights {
                run,
                collect,
                deliver,
                poll,
            }) => write!(
                formatter,
                "walk(run={run} collect={collect} deliver={deliver} poll={poll})"
            ),
            Self::Pct { depth, horizon } => write!(formatter, "pct(d={depth} k={horizon})"),
        }
    }
}

pub struct Scheduler {
    policy: Policy,
    /// Each actor's priority under PCT; higher runs first.
    priorities: BTreeMap<Actor, i64>,
    /// The steps at which the actor that runs drops, earliest first.
    change_points: Vec<u64>,
    /// The priority the next actor to drop gets, below every other.
    floor: i64,
}

/// Where PCT's initial priorities start, above every dropped actor's.
const HIGH: i64 = 1 << 32;

impl Scheduler {
    pub fn new(policy: Policy, choices: &mut Choices) -> Self {
        let mut change_points = match policy {
            Policy::Walk(_) => Vec::new(),
            Policy::Pct { depth, horizon } => (1..depth)
                .map(|_| choices.below(Stream::Schedule, horizon) + 1)
                .collect(),
        };
        change_points.sort_unstable();
        Self {
            policy,
            priorities: BTreeMap::new(),
            change_points,
            floor: 0,
        }
    }

    /// Chooses the action taken at `step` from those enabled.
    pub fn choose(&mut self, enabled: &[Action], step: u64, choices: &mut Choices) -> Action {
        match self.policy {
            Policy::Walk(weights) => walk(weights, enabled, choices),
            Policy::Pct { .. } => {
                for action in enabled {
                    // A new actor takes a random place among the others.
                    self.priorities.entry(action.actor()).or_insert_with(|| {
                        HIGH + i64::try_from(choices.below(Stream::Schedule, 1 << 32))
                            .expect("a priority fits")
                    });
                }
                let action = *enabled
                    .iter()
                    .max_by_key(|action| (self.priorities[&action.actor()], action.actor()))
                    .expect("an action is enabled");
                if self.change_points.first() == Some(&step) {
                    self.change_points.remove(0);
                    self.drop_actor(action.actor());
                }
                action
            }
        }
    }

    /// A thread gave up the CPU: under PCT, every other actor goes first.
    pub fn yielded(&mut self, tid: Tid) {
        if matches!(self.policy, Policy::Pct { .. }) {
            self.drop_actor(Actor::Thread(tid));
        }
    }

    fn drop_actor(&mut self, actor: Actor) {
        self.floor -= 1;
        self.priorities.insert(actor, self.floor);
    }
}

/// A random walk: an action kind by weight, then one action of that kind.
fn walk(weights: Weights, enabled: &[Action], choices: &mut Choices) -> Action {
    let weights = [weights.run, weights.collect, weights.deliver, weights.poll];
    let kinds = std::array::from_fn::<_, 4, _>(|kind| {
        if enabled.iter().any(|action| action.kind() == kind) {
            weights[kind]
        } else {
            0
        }
    });
    let kind = choices.weighted(Stream::Schedule, &kinds);
    let of_kind = enabled
        .iter()
        .filter(|action| action.kind() == kind)
        .copied()
        .collect::<Vec<_>>();
    match of_kind.as_slice() {
        [only] => *only,
        _ => *choices.pick(Stream::Schedule, &of_kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under PCT the highest-priority enabled actor runs until a change
    /// point or a yield drops it below every other, and an actor that
    /// dropped later sits lower.
    #[test]
    fn pct_runs_the_highest_priority_actor_until_it_drops() {
        let mut choices = Choices::new(3);
        let mut scheduler = Scheduler::new(
            Policy::Pct {
                depth: 2,
                horizon: 10,
            },
            &mut choices,
        );
        let change = scheduler.change_points[0];
        let enabled = [Action::Run(1), Action::Run(2), Action::Run(3)];
        let first = scheduler.choose(&enabled, 1, &mut choices);
        for step in 2..=change {
            assert_eq!(scheduler.choose(&enabled, step, &mut choices), first);
        }
        // The change point dropped the actor that ran at it.
        let second = scheduler.choose(&enabled, change + 1, &mut choices);
        assert_ne!(second, first);
        let Action::Run(tid) = second else {
            unreachable!("only threads are enabled")
        };
        scheduler.yielded(tid);
        let third = scheduler.choose(&enabled, change + 2, &mut choices);
        assert!(![first, second].contains(&third));
        let Action::Run(tid) = third else {
            unreachable!("only threads are enabled")
        };
        // Once the third yields too, the earliest to drop is highest again.
        scheduler.yielded(tid);
        assert_eq!(scheduler.choose(&enabled, change + 3, &mut choices), first);
    }
}
