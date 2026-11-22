// SPDX-License-Identifier: GPL-2.0-only

//! Locks that are safe to use together with interrupts.
//!
//! A plain spinlock deadlocks in this situation: code takes the lock, an interrupt arrives on
//! the same CPU, and the interrupt handler tries to take the same lock — it spins for ever,
//! because the owner cannot continue until the handler returns. [`IrqMutex`] prevents that by
//! switching interrupts off on this CPU while the lock is held, and restoring the previous
//! state when the guard is dropped.

use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

use crate::arch::interrupts;

/// A spinlock that keeps interrupts off on this CPU while it is held.
pub struct IrqMutex<T> {
    inner: spin::Mutex<T>,
}

impl<T> IrqMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: spin::Mutex::new(value),
        }
    }

    pub fn lock(&self) -> IrqMutexGuard<'_, T> {
        // Interrupts go off *before* spinning: otherwise an interrupt could arrive between
        // taking the lock and switching them off.
        let were_enabled = interrupts::disable();
        IrqMutexGuard {
            guard: ManuallyDrop::new(self.inner.lock()),
            were_enabled,
        }
    }
}

/// Access to the data of a locked [`IrqMutex`]; unlocking happens on drop.
pub struct IrqMutexGuard<'a, T> {
    guard: ManuallyDrop<spin::MutexGuard<'a, T>>,
    were_enabled: bool,
}

impl<T> Deref for IrqMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for IrqMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for IrqMutexGuard<'_, T> {
    fn drop(&mut self) {
        // Release the lock first, then let interrupts in again: the reverse of `lock`.
        // SAFETY: `guard` is dropped exactly once, here, and never used afterwards.
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        if self.were_enabled {
            interrupts::enable();
        }
    }
}
