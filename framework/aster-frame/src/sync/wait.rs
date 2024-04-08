// SPDX-License-Identifier: MPL-2.0

use alloc::{collections::VecDeque, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use bitflags::bitflags;

use super::SpinLock;
use crate::{
    arch::timer::{add_timeout_list, TIMER_FREQ},
    task::{add_task, current_task, with_current, yield_now, Task, WakeUp},
    trap::disable_local,
};

/// A wait queue.
///
/// One may wait on a wait queue to put its executing thread to sleep.
/// Multiple threads may be the waiters of a wait queue.
/// Other threads may invoke the `wake`-family methods of a wait queue to
/// wake up one or many waiter threads.
pub struct WaitQueue {
    waiters: SpinLock<VecDeque<Arc<Waiter>>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        WaitQueue {
            waiters: SpinLock::new(VecDeque::new()),
        }
    }

    /// Wait until some condition becomes true.
    ///
    /// This method takes a closure that tests a user-given condition.
    /// The method only returns if the condition returns Some(_).
    /// A waker thread should first make the condition Some(_), then invoke the
    /// `wake`-family method. This ordering is important to ensure that waiter
    /// threads do not lose any wakeup notifiations.
    ///
    /// By taking a condition closure, his wait-wakeup mechanism becomes
    /// more efficient and robust.
    pub fn wait_until<F, R>(&self, cond: F) -> R
    where
        F: FnMut() -> Option<R>,
    {
        self.do_wait(cond, None).unwrap()
    }

    /// Wait until some condition returns Some(_), or a given timeout is reached. If
    /// the condition does not becomes Some(_) before the timeout is reached, the
    /// function will return None.
    pub fn wait_until_or_timeout<F, R>(&self, cond: F, timeout: &Duration) -> Option<R>
    where
        F: FnMut() -> Option<R>,
    {
        self.do_wait(cond, Some(timeout))
    }

    fn do_wait<F, R>(&self, mut cond: F, timeout: Option<&Duration>) -> Option<R>
    where
        F: FnMut() -> Option<R>,
    {
        let waiter = loop {
            if let Some(res) = cond() {
                return Some(res);
            }

            if let Some(waiter) = Waiter::new() {
                break Arc::new(waiter);
            }
            yield_now();
        };

        self.enqueue(&waiter);

        let timer_callback = timeout.map(|timeout| {
            let remaining_ticks = {
                // FIXME: We currently require 1000 to be a multiple of TIMER_FREQ, but
                // this may not hold true in the future, because TIMER_FREQ can be greater
                // than 1000. Then, the code need to be refactored.
                const_assert!(1000000 % TIMER_FREQ == 0);

                let us_per_tick = 1000000 / TIMER_FREQ;

                // The ticks should be equal to or greater than timeout
                (timeout.as_micros() as u64 + us_per_tick - 1) / us_per_tick
            };

            add_timeout_list(remaining_ticks, waiter.clone(), |timer_call_back| {
                let waiter = timer_call_back
                    .data()
                    .downcast_ref::<Arc<Waiter>>()
                    .unwrap();
                waiter.wake_up();
            })
        });

        loop {
            if let Some(res) = cond() {
                self.dequeue(&waiter);

                if let Some(timer_callback) = timer_callback {
                    timer_callback.cancel();
                }

                return Some(res);
            };

            if let Some(ref timer_callback) = timer_callback
                && timer_callback.is_expired()
            {
                self.dequeue(&waiter);
                return cond();
            }

            waiter.wait();
        }
    }

    /// Wake one waiter thread, if there is one.
    pub fn wake_one(&self) {
        if let Some(waiter) = self.waiters.lock_irq_disabled().front() {
            waiter.wake_up();
        }
    }

    /// Wake all not-exclusive waiter threads and at most one exclusive waiter.
    pub fn wake_all(&self) {
        for waiter in self.waiters.lock_irq_disabled().iter() {
            waiter.wake_up();
            if waiter.is_exclusive() {
                break;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.waiters.lock_irq_disabled().is_empty()
    }

    // Enqueue a waiter into current waitqueue. If waiter is exclusive, add to the back of waitqueue.
    // Otherwise, add to the front of waitqueue
    fn enqueue(&self, waiter: &Arc<Waiter>) {
        if waiter.is_exclusive() {
            self.waiters.lock_irq_disabled().push_back(waiter.clone())
        } else {
            self.waiters.lock_irq_disabled().push_front(waiter.clone());
        }
    }

    fn dequeue(&self, waiter: &Arc<Waiter>) {
        self.waiters
            .lock_irq_disabled()
            .retain(|waiter_| !Arc::ptr_eq(waiter_, waiter))
    }
}

pub struct Waiter {
    /// Whether the waiter is woken_up
    is_woken_up: AtomicBool,
    /// To respect different wait condition
    flag: WaiterFlag,
    /// The `Task` held by the waiter.
    task: Arc<Task>,
}

impl PartialEq for Waiter {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.task, &other.task)
    }
}

impl core::fmt::Debug for Waiter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Waiter")
            .field(&Arc::as_ptr(&self.task))
            .finish()
    }
}

impl Waiter {
    pub fn new() -> Option<Self> {
        current_task().map(|task| Waiter {
            is_woken_up: AtomicBool::new(false),
            flag: WaiterFlag::empty(),
            task,
        })
    }

    /// make self into wait status until be called wake up
    pub fn wait(&self) {
        with_current(|cur| assert_eq!(cur.as_ptr(), Arc::as_ptr(&self.task)));
        self.task.sleep();
        while !self.is_woken_up.load(Ordering::SeqCst) {
            yield_now();
        }
        self.is_woken_up.store(false, Ordering::SeqCst);
    }

    pub fn wake_up(&self) {
        if let Ok(false) =
            self.is_woken_up
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            let guard = disable_local();
            if self.task.wakeup() {
                add_task(self.task.clone());
            }
        }
    }

    pub fn is_exclusive(&self) -> bool {
        self.flag.contains(WaiterFlag::EXCLUSIVE)
    }
}

bitflags! {
    pub struct WaiterFlag: u32 {
        const EXCLUSIVE         = 1 << 0;
    }
}
