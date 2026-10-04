//! A run's shape, chosen from its seed before the session starts.
//!
//! Swarm testing (Groce et al.) varies which features are active and how
//! intensely, rather than enabling everything always, because some bugs
//! only appear when other activity stays quiet.

use std::fmt;

use super::choices::{Choices, Stream};
use super::corpus::Corpus;

/// How likely each kind of action is, relative to the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    pub run: u64,
    pub collect: u64,
    pub deliver: u64,
    pub poll: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swarm {
    pub program: usize,
    pub variant: usize,
    /// Which of the program's manifest runs, and so which arguments.
    pub run: usize,
    pub weights: Weights,
    /// The most instructions one `Run` action executes.
    pub burst: u64,
    /// The controller queue's capacity.
    pub queue_capacity: usize,
    /// The event channel's capacity; small ones make clients lag.
    pub event_capacity: usize,
    /// Whether launches stop at the program's first instruction.
    pub stop_at_entry: bool,
    /// How many requests the client makes before it shuts down.
    pub requests: u64,
    /// How many times the client may launch the program.
    pub launches: u64,
    /// Breakpoints the client adds before its first launch.
    pub early_breakpoints: u64,
}

impl Swarm {
    /// Chooses a run's shape from the `Swarm` stream.
    pub fn choose(choices: &mut Choices, corpus: &Corpus) -> Self {
        let mut index = |count: usize| {
            usize::try_from(choices.below(Stream::Swarm, count as u64)).expect("an index fits")
        };
        let program = index(corpus.programs.len());
        let variant = index(corpus.programs[program].variants.len());
        let run = index(corpus.programs[program].runs.len());
        let mut pick = |options: &[u64]| *choices.pick(Stream::Swarm, options);
        let weight = [1, 4, 16];
        let weights = Weights {
            run: pick(&weight),
            collect: pick(&weight),
            deliver: pick(&weight),
            poll: pick(&weight),
        };
        Self {
            program,
            variant,
            run,
            weights,
            burst: pick(&[1, 8, 64, 512]),
            queue_capacity: usize::try_from(pick(&[1, 2, 8, 32])).expect("small"),
            event_capacity: usize::try_from(pick(&[2, 16, 1024])).expect("small"),
            stop_at_entry: pick(&[0, 1]) == 1,
            requests: pick(&[4, 16, 48]),
            launches: pick(&[1, 2]),
            early_breakpoints: pick(&[0, 1, 3]),
        }
    }
}

impl fmt::Display for Swarm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Weights {
            run,
            collect,
            deliver,
            poll,
        } = self.weights;
        write!(
            formatter,
            "weights run={run} collect={collect} deliver={deliver} poll={poll} burst={} \
             queue={} events={} entry={} requests={} launches={} early-breakpoints={}",
            self.burst,
            self.queue_capacity,
            self.event_capacity,
            self.stop_at_entry,
            self.requests,
            self.launches,
            self.early_breakpoints,
        )
    }
}
