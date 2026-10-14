//! Loom's `Mutex` and `Condvar`, with a scheduling point at each unlock.
//!
//! Loom switches threads only before operations that can block or observe
//! another thread, such as taking a lock; it never does so on an unlock.
//! A protocol that hands work off through `try_lock` depends on what
//! another thread does between one unlock and the next: a worker that
//! looks for more work, then lets go of the lock, strands whatever arrived
//! in between. Loom alone never runs a thread in that gap, so a model of
//! the protocol passes with the bug in it.
//!
//! Here each unlock is followed by a read-modify-write of an atomic the
//! mutex owns, which every lock and `try_lock` also touches first. The
//! accesses conflict, so loom also tries running the other thread right
//! after the unlock.

use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, PoisonError, TryLockError, TryLockResult};

use loom::sync::atomic::{AtomicUsize, Ordering};

pub struct Mutex<T> {
    inner: loom::sync::Mutex<T>,
    /// Touched around every lock and unlock, for loom to schedule at.
    turns: AtomicUsize,
}

pub struct MutexGuard<'a, T> {
    inner: Option<loom::sync::MutexGuard<'a, T>>,
    turns: &'a AtomicUsize,
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: loom::sync::Mutex::new(value),
            turns: AtomicUsize::new(0),
        }
    }

    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        self.turns.fetch_add(1, Ordering::Relaxed);
        Ok(self.guard(self.inner.lock().unwrap_or_else(PoisonError::into_inner)))
    }

    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        self.turns.fetch_add(1, Ordering::Relaxed);
        match self.inner.try_lock() {
            Ok(inner) => Ok(self.guard(inner)),
            Err(TryLockError::Poisoned(poisoned)) => Ok(self.guard(poisoned.into_inner())),
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
        }
    }

    pub fn into_inner(self) -> LockResult<T> {
        self.inner.into_inner()
    }

    fn guard<'a>(&'a self, inner: loom::sync::MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        MutexGuard {
            inner: Some(inner),
            turns: &self.turns,
        }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.inner
            .as_deref()
            .expect("a guard holds its lock until dropped")
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner
            .as_deref_mut()
            .expect("a guard holds its lock until dropped")
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.inner.take().is_some() {
            self.turns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub struct Condvar {
    inner: loom::sync::Condvar,
}

impl Condvar {
    pub fn new() -> Self {
        Self {
            inner: loom::sync::Condvar::new(),
        }
    }

    /// Waits as loom's `Condvar` does, unlocking while it waits.
    pub fn wait<'a, T>(&self, mut guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        let turns = guard.turns;
        let inner = guard
            .inner
            .take()
            .expect("a guard holds its lock until dropped");
        let inner = self
            .inner
            .wait(inner)
            .unwrap_or_else(PoisonError::into_inner);
        Ok(MutexGuard {
            inner: Some(inner),
            turns,
        })
    }

    pub fn notify_all(&self) {
        self.inner.notify_all();
    }
}
