// SPDX-License-Identifier: MPL-2.0

use core::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::trap::{disable_local, DisabledLocalIrqGuard};

/// A spin lock.
pub struct SpinLock<T: ?Sized> {
    lock: AtomicBool,
    val: UnsafeCell<T>,
}

impl<T> SpinLock<T> {
    /// Creates a new spin lock.
    pub const fn new(val: T) -> Self {
        Self {
            val: UnsafeCell::new(val),
            lock: AtomicBool::new(false),
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.val.get_mut()
    }

    pub fn as_ptr(&self) -> *mut T {
        self.val.get()
    }
}

impl<T: ?Sized> SpinLock<T> {
    pub fn is_locked(&self) -> bool {
        self.lock.load(Ordering::Relaxed)
    }

    /// Acquire the spin lock with disabling the local IRQs. This is the most secure
    /// locking way.
    ///
    /// This method runs in a busy loop until the lock can be acquired.
    /// After acquiring the spin lock, all interrupts are disabled.
    #[track_caller]
    pub fn lock_irq_disabled(&self) -> SpinLockGuard<T> {
        let guard = disable_local();
        self.acquire_lock();
        SpinLockGuard {
            lock: self,
            inner_guard: InnerGuard::IrqGuard(guard),
        }
    }

    /// Try acquiring the spin lock immedidately with disabling the local IRQs.
    #[track_caller]
    pub fn try_lock_irq_disabled(&self) -> Option<SpinLockGuard<T>> {
        let irq_guard = disable_local();
        if self.try_acquire_lock() {
            let lock_guard = SpinLockGuard {
                lock: self,
                inner_guard: InnerGuard::IrqGuard(irq_guard),
            };
            return Some(lock_guard);
        }
        None
    }

    /// Acquire the spin lock without disabling local IRQs.
    ///
    /// This method is twice as fast as the `lock_irq_disabled` method.
    /// So prefer using this method over the `lock_irq_disabled` method
    /// when IRQ handlers are allowed to get executed while
    /// holding this lock. For example, if a lock is never used
    /// in the interrupt context, then it is ok to use this method
    /// in the process context.
    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<T> {
        self.acquire_lock();
        SpinLockGuard {
            lock: self,
            inner_guard: InnerGuard::None,
        }
    }

    /// Try acquiring the spin lock immedidately without disabling the local IRQs.
    pub fn try_lock(&self) -> Option<SpinLockGuard<T>> {
        if self.try_acquire_lock() {
            let lock_guard = SpinLockGuard {
                lock: self,
                inner_guard: InnerGuard::None,
            };
            return Some(lock_guard);
        }
        None
    }

    /// Access the spin lock, otherwise busy waiting
    #[track_caller]
    fn acquire_lock(&self) {
        let mut spin_shift = 0;
        let mut stuck_count = 0;
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Acquire)
            .is_err()
        {
            for _ in 0..(1 << spin_shift) {
                core::hint::spin_loop();
            }
            if spin_shift < 16 {
                spin_shift += 1;
            } else {
                stuck_count += 1;
                if stuck_count % 5000 == 0 {
                    crate::early_println!(
                        "CPU#{} stuck in {}",
                        crate::cpu::this_cpu(),
                        core::panic::Location::caller()
                    );
                }
            }
        }
    }

    fn try_acquire_lock(&self) -> bool {
        self.lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Acquire)
            .is_ok()
    }

    fn release_lock(&self) {
        self.lock.store(false, Ordering::Release);
    }
}

impl<T: fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.val, f)
    }
}

// Safety. Only a single lock holder is permitted to access the inner data of Spinlock.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

enum InnerGuard {
    IrqGuard(DisabledLocalIrqGuard),
    None,
}

/// The guard of a spin lock that disables the local IRQs.
pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
    inner_guard: InnerGuard,
}

impl<'a, T: ?Sized> Deref for SpinLockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &mut *self.lock.val.get() }
    }
}

impl<'a, T: ?Sized> DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.val.get() }
    }
}

impl<'a, T: ?Sized> Drop for SpinLockGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.release_lock();
    }
}

impl<'a, T: ?Sized + fmt::Debug> fmt::Debug for SpinLockGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'a, T: ?Sized> !Send for SpinLockGuard<'a, T> {}

// Safety. `SpinLockGuard` can be shared between tasks/threads in same CPU.
// As `lock()` is only called when there are no race conditions caused by interrupts.
unsafe impl<T: ?Sized + Sync> Sync for SpinLockGuard<'_, T> {}
