//! Identities of function activations, and of positions on their stacks,
//! that stay comparable from one stop to the next.
//!
//! Run control remembers activations across resumes: the frame a step
//! began in, the caller it returned to, the frame a watched local lives
//! in. An activation is named by its canonical frame address, but only
//! relative to its stack, and only through these types, so that run
//! control never compares raw stack addresses itself.

use nix::unistd::Pid;

use crate::VirtualAddress;

/// The stack an activation or position belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StackOwner {
    /// An operating-system thread's own stack, which never moves.
    Thread(Pid),
}

/// How deep in its stack a position lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// An address on a stack that never moves. x86-64 stacks grow down, so
    /// a lower address is deeper.
    Address(u64),
}

impl Depth {
    /// Whether this depth is strictly deeper than `other` on one stack.
    const fn is_deeper_than(self, other: Self) -> bool {
        match (self, other) {
            (Self::Address(this), Self::Address(other)) => this < other,
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

/// One stop's view of a stack, which turns its addresses into activations
/// and positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StackView {
    owner: StackOwner,
}

impl StackView {
    /// A thread's own stack.
    pub(super) const fn thread(pid: Pid) -> Self {
        Self {
            owner: StackOwner::Thread(pid),
        }
    }

    /// The activation whose canonical frame address is `cfa`.
    pub(super) const fn activation(self, cfa: VirtualAddress) -> Activation {
        Activation {
            owner: self.owner,
            depth: Depth::Address(cfa.get()),
        }
    }

    /// The position of a stack pointer.
    pub(super) const fn position(self, stack_pointer: u64) -> StackPosition {
        StackPosition {
            owner: self.owner,
            depth: Depth::Address(stack_pointer),
        }
    }

    /// The canonical frame address `activation` has at this stop, or `None`
    /// for an activation of another stack.
    pub(super) fn cfa_of(self, activation: Activation) -> Option<VirtualAddress> {
        (activation.owner == self.owner).then(|| match activation.depth {
            Depth::Address(address) => VirtualAddress::new(address),
        })
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
}
