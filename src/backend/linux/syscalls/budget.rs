//! Holds the BPF engine's cost to a share of one CPU.
//!
//! Once a second the reader says what tracing cost; over the budget, the
//! engine gives up detail in a fixed order, one step a second, and never
//! takes it back.

use std::collections::VecDeque;

/// The most bytes of a string or buffer kept once strings are capped.
pub const CAPPED_BYTES: u32 = 16;

/// The most elements of an array kept once strings are capped.
pub const CAPPED_ELEMENTS: u32 = 4;

/// The sparsest sampling before only counting: one call in this many.
pub const SPARSEST: u32 = 64;

/// What the engine gives up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    DropStacks,
    CapStrings,

    /// Send one call in this many, and count the rest.
    Sample(u32),
    CountOnly,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DropStacks => write!(f, "stacks dropped"),
            Self::CapStrings => write!(
                f,
                "strings and buffers cut at {CAPPED_BYTES} bytes, arrays at {CAPPED_ELEMENTS}"
            ),
            Self::Sample(every) => write!(f, "one call in {every} shown, the rest counted"),
            Self::CountOnly => write!(f, "calls counted, none shown"),
        }
    }
}

/// The steps left, and the share of a CPU they hold tracing to.
#[derive(Debug)]
pub struct Budget {
    /// Percent of one CPU.
    percent: f64,
    steps: VecDeque<Step>,
}

impl Budget {
    /// A budget of `percent` of one CPU, for a trace that takes stacks or
    /// copies more than the capped bytes as asked.
    #[must_use]
    pub fn new(percent: f64, stacks: bool, long_strings: bool) -> Self {
        let mut steps = VecDeque::new();
        if stacks {
            steps.push_back(Step::DropStacks);
        }

        if long_strings {
            steps.push_back(Step::CapStrings);
        }

        let mut every = 2;
        while every <= SPARSEST {
            steps.push_back(Step::Sample(every));
            every *= 2;
        }

        steps.push_back(Step::CountOnly);
        Self { percent, steps }
    }

    /// The step to take after tracing used `used` nanoseconds of CPU over
    /// `window` nanoseconds, if that was over the budget.
    pub fn after(&mut self, used: u64, window: u64) -> Option<Step> {
        if share(used, window) > self.percent {
            self.steps.pop_front()
        } else {
            None
        }
    }
}

/// The percent of one CPU that `used` nanoseconds of it are over `window`.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "what is lost above 2^53 ns, about 104 days, is of no account"
)]
pub fn share(used: u64, window: u64) -> f64 {
    used as f64 * 100.0 / window.max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_budget_gives_up_detail_in_order_and_under_it_keeps_it() {
        let second = 1_000_000_000;
        let mut budget = Budget::new(2.0, true, true);
        assert_eq!(budget.after(20_000_000, second), None, "at the budget");
        let mut taken = Vec::new();
        while let Some(step) = budget.after(30_000_000, second) {
            taken.push(step);
        }

        assert_eq!(
            taken,
            [
                Step::DropStacks,
                Step::CapStrings,
                Step::Sample(2),
                Step::Sample(4),
                Step::Sample(8),
                Step::Sample(16),
                Step::Sample(32),
                Step::Sample(64),
                Step::CountOnly,
            ]
        );

        // Nothing to drop is not a step.
        let mut plain = Budget::new(2.0, false, false);
        assert_eq!(plain.after(30_000_000, second), Some(Step::Sample(2)));
    }
}
