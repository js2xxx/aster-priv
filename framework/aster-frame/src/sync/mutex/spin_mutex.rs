use core::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

/// A mutex.
pub struct Mutex<T> {
    val: UnsafeCell<T>,
    lock: AtomicBool,
}

impl<T> Mutex<T> {
    /// Create a new mutex.
    pub const fn new(val: T) -> Self {
        Self {
            val: UnsafeCell::new(val),
            lock: AtomicBool::new(false),
        }
    }

    pub const fn as_ptr(&self) -> *mut T {
        self.val.get()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.val.get_mut()
    }

    /// Acquire the mutex.
    ///
    /// This method runs in a block way until the mutex can be acquired.
    pub fn lock(&self) -> MutexGuard<T> {
        loop {
            let local = crate::trap::disable_local();
            let mut spin_count = 0;
            while spin_count < 11 {
                if let Some(guard) = self.try_lock() {
                    return guard;
                }
                for _ in 0..(1 << spin_count) {
                    core::hint::spin_loop();
                }
                spin_count += 1;
            }
            drop(local);
            crate::task::yield_now();
        }
    }

    /// Try Acquire the mutex immedidately.
    pub fn try_lock(&self) -> Option<MutexGuard<T>> {
        // Cannot be reduced to `then_some`, or the possible dropping of the temporary
        // guard will cause an unexpected unlock.
        self.acquire_lock().then(|| MutexGuard::new(self))
    }

    /// Release the mutex and wake up one thread which is blocked on this mutex.
    fn unlock(&self) {
        self.release_lock();
        crate::task::schedule();
    }

    fn acquire_lock(&self) -> bool {
        self.lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn release_lock(&self) {
        self.lock.store(false, Ordering::Release);
    }
}

impl<T: fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.val, f)
    }
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

#[clippy::has_significant_drop]
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<'a, T> MutexGuard<'a, T> {
    fn new(mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        MutexGuard { mutex }
    }
}

impl<'a, T> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.val.get() }
    }
}

impl<'a, T> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.val.get() }
    }
}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<'a, T: fmt::Debug> fmt::Debug for MutexGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'a, T> !Send for MutexGuard<'a, T> {}

unsafe impl<T: Sync> Sync for MutexGuard<'_, T> {}
