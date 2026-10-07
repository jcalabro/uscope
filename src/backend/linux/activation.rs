//! Identities of function activations, and of positions on their stacks,
//! that stay comparable from one stop to the next.
//!
//! Run control remembers activations across resumes: the frame a step
//! began in, the caller it returned to, the frame a watched local lives
//! in. An activation is named by its canonical frame address, but only
//! relative to its stack, and only through these types, so that run
//! control never compares raw stack addresses itself.
//!
//! A task's stack may move: Go's runtime copies a goroutine's stack to grow
//! or shrink it. A position on such a stack is measured from the stack's
//! top, which moves with it, and the stack's bounds are read again at each
//! stop.

use nix::unistd::Pid;

use crate::{TaskId, VirtualAddress};

/// The stack an activation or position belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StackOwner {
    /// An operating-system thread's own stack, which never moves.
    Thread(Pid),
    /// A task's stack, which its runtime may move.
    Task(TaskId),
}

/// How deep in its stack a position lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// An address on a stack that never moves. x86-64 stacks grow down, so
    /// a lower address is deeper.
    Address(u64),
    /// Bytes below the top of a stack that may move, so a larger offset is
    /// deeper.
    BelowTop(u64),
}

impl Depth {
    /// Whether this depth is strictly deeper than `other` on one stack.
    const fn is_deeper_than(self, other: Self) -> bool {
        match (self, other) {
            (Self::Address(this), Self::Address(other)) => this < other,
            (Self::BelowTop(this), Self::BelowTop(other)) => this > other,
            // One stack is measured one way.
            (Self::Address(_), Self::BelowTop(_)) | (Self::BelowTop(_), Self::Address(_)) => false,
        }
    }
}

/// One function activation: the frame whose canonical frame address marks
/// it on its stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Activation {
    owner: StackOwner,
    depth: Depth,
}

impl Activation {
    /// Whether this activation is deeper on the same stack than `other`: a
    /// callee `other` made, directly or through others.
    pub(super) fn is_callee_of(self, other: Self) -> bool {
        self.owner == other.owner && self.depth.is_deeper_than(other.depth)
    }

    /// Whether a stack pointer at `position` shows this activation has
    /// returned: its stack was popped to the canonical frame address or
    /// beyond, which only the return does.
    pub(super) fn has_returned(self, position: StackPosition) -> bool {
        self.owner == position.owner && !position.depth.is_deeper_than(self.depth)
    }

    /// Whether a stack pointer at `position` is where this activation's
    /// return leaves it: popped to its canonical frame address exactly, as
    /// no frame further out returning is.
    pub(super) fn just_returned(self, position: StackPosition) -> bool {
        self.owner == position.owner && self.depth == position.depth
    }
}

/// A stack pointer's position on its stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StackPosition {
    owner: StackOwner,
    depth: Depth,
}

impl StackPosition {
    /// Whether this position is strictly deeper than `other` on one stack,
    /// as a callee's stack pointer is.
    pub(super) fn is_deeper_than(self, other: Self) -> bool {
        self.owner == other.owner && self.depth.is_deeper_than(other.depth)
    }
}

/// A task's stack as one stop finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TaskStack {
    pub(super) task: TaskId,
    /// The stack's lowest address.
    pub(super) low: u64,
    /// The address just above the stack, its top.
    pub(super) high: u64,
}

/// One stop's view of the stacks a thread runs on, which turns their
/// addresses into activations and positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StackView {
    thread: Pid,
    /// The task the thread runs, whose own stack holds the addresses within
    /// its bounds; the thread's stacks hold every other.
    task: Option<TaskStack>,
}

impl StackView {
    /// A thread's own stacks.
    pub(super) const fn thread(pid: Pid) -> Self {
        Self {
            thread: pid,
            task: None,
        }
    }

    /// A thread's stacks while it runs a task on the task's own stack.
    pub(super) const fn task(pid: Pid, task: TaskStack) -> Self {
        Self {
            thread: pid,
            task: Some(task),
        }
    }

    /// Whose stack holds `address`, and how deep in it.
    const fn locate(self, address: u64) -> (StackOwner, Depth) {
        match self.task {
            Some(stack) if stack.low <= address && address <= stack.high => (
                StackOwner::Task(stack.task),
                Depth::BelowTop(stack.high - address),
            ),
            _ => (StackOwner::Thread(self.thread), Depth::Address(address)),
        }
    }

    /// The activation whose canonical frame address is `cfa`.
    pub(super) const fn activation(self, cfa: VirtualAddress) -> Activation {
        let (owner, depth) = self.locate(cfa.get());
        Activation { owner, depth }
    }

    /// The position of a stack pointer.
    pub(super) const fn position(self, stack_pointer: u64) -> StackPosition {
        let (owner, depth) = self.locate(stack_pointer);
        StackPosition { owner, depth }
    }

    /// The canonical frame address `activation` has at this stop, or `None`
    /// for an activation of a stack this view does not see.
    pub(super) fn cfa_of(self, activation: Activation) -> Option<VirtualAddress> {
        match (activation.owner, activation.depth, self.task) {
            (StackOwner::Thread(pid), Depth::Address(address), _) if pid == self.thread => {
                Some(VirtualAddress::new(address))
            }
            (StackOwner::Task(task), Depth::BelowTop(offset), Some(stack))
                if task == stack.task =>
            {
                stack.high.checked_sub(offset).map(VirtualAddress::new)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activations_compare_only_on_their_own_stack() {
        let stack = StackView::thread(Pid::from_raw(1));
        let other = StackView::thread(Pid::from_raw(2));
        let outer = stack.activation(VirtualAddress::new(0x1000));
        let inner = stack.activation(VirtualAddress::new(0x0f00));
        assert!(inner.is_callee_of(outer));
        assert!(!outer.is_callee_of(inner));
        assert!(!inner.is_callee_of(inner));
        assert!(
            !other
                .activation(VirtualAddress::new(0x0f00))
                .is_callee_of(outer)
        );

        // A frame has returned once its stack is popped to its CFA.
        assert!(!inner.has_returned(stack.position(0x0ef8)));
        assert!(inner.has_returned(stack.position(0x0f00)));
        assert!(inner.has_returned(stack.position(0x0f08)));
        assert!(!inner.has_returned(other.position(0x0f08)));
        assert!(
            stack
                .position(0x0ef8)
                .is_deeper_than(stack.position(0x0f00))
        );
    }

    #[test]
    fn a_task_s_activations_survive_its_stack_moving() {
        let task = TaskId {
            runtime: crate::RuntimeId::new(1),
            number: 7,
        };
        let thread = Pid::from_raw(1);
        let before = StackView::task(
            thread,
            TaskStack {
                task,
                low: 0x1_0000,
                high: 0x1_2000,
            },
        );
        // The runtime copied the stack to a larger one, on another thread.
        let after = StackView::task(
            Pid::from_raw(2),
            TaskStack {
                task,
                low: 0x5_0000,
                high: 0x5_4000,
            },
        );
        let frame = before.activation(VirtualAddress::new(0x1_1f00));
        assert_eq!(after.activation(VirtualAddress::new(0x5_3f00)), frame);
        assert_eq!(after.cfa_of(frame), Some(VirtualAddress::new(0x5_3f00)));
        assert!(
            after
                .activation(VirtualAddress::new(0x5_3e00))
                .is_callee_of(frame)
        );
        assert!(!frame.has_returned(after.position(0x5_3ef8)));
        assert!(frame.has_returned(after.position(0x5_3f08)));

        // The thread's system stack is no part of the task's.
        let system = after.activation(VirtualAddress::new(0x9_0000));
        assert!(!system.is_callee_of(frame) && !frame.is_callee_of(system));
        assert!(!frame.has_returned(after.position(0x9_0000)));
        assert_eq!(before.cfa_of(system), None);
    }
}
