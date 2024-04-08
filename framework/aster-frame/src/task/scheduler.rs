// SPDX-License-Identifier: MPL-2.0

use super::{task::SchedTask, DisablePreemptGuard};
use crate::{
    prelude::*,
    task::{preempt::is_in_preemption, task::NeedResched, with_current, Task},
};

pub(crate) static GLOBAL_SCHEDULER: GlobalScheduler = GlobalScheduler::new();

/// Logs the scheduler debug information.
///
/// Turn on the `SCHED_DEBUG_LOG` in `config.rs` to enable this macro.
/// The logs will be outputted at the [`Debug`] level.
///
/// [`Debug`]: log::Level::Debug
///
/// # Examples
///
/// ```
/// sched_debug!("{} debug information", 1);
/// ```
///
#[macro_export]
macro_rules! sched_debug {
    () => {};
    ($fmt: literal $(, $($arg: tt)+)?) => {
        if $crate::config::SCHED_DEBUG_LOG {
            log::debug!($fmt $(, $($arg)+)?);
        }
    }
}

/// A scheduler for tasks.
///
/// Operations on the scheduler should be performed with interrupts disabled,
/// which has been ensured by the callers of the `GLOBAL_SCHEDULER`.
/// Therefore, implementations of this trait do not need to worry about interrupt safety.
pub trait Scheduler: Sync + Send {
    /// Add the task to the scheduler when it enters a runnable state.
    fn enqueue(&self, task: SchedTask);

    /// Pick the most appropriate task eligible to run next from the scheduler.
    fn pick_next_task(&self) -> Option<SchedTask>;

    /// Remove the task-related from the scheduler when the task is no longer alive.
    fn clear(&self, task: &Task);

    /// Tells whether the current task should be preempted by tasks in the queue.
    ///
    /// # Panics
    ///
    /// Panics if the current task is none.
    fn should_preempt_cur_task(&self) -> bool;

    /// Handle a tick from the timer.
    /// Modify the states of the current task on the processor
    /// according to the time update.
    ///
    /// # Panics
    ///
    /// Panics if the current task is none.
    fn tick_cur_task(&self);

    /// Modify states before yielding the current task.
    /// Set the `need_resched` flag of the current task.
    ///
    /// # Panics
    ///
    /// Panics if the current task is none.
    fn prepare_to_yield_cur_task(&self) {
        with_current(|cur_task| {
            cur_task.set_need_resched(true);
            sched_debug!("before yield: {:p}", cur_task.as_ptr());
        });
    }

    fn prepare_to_yield_to(&self, task: &SchedTask);

    fn load_balance(&self) {}

    fn traverse(&self) {}
}

pub struct GlobalScheduler {
    scheduler: spin::Once<&'static dyn Scheduler>,
    // TODO: add multiple scheduler management
}

impl GlobalScheduler {
    pub const fn new() -> Self {
        Self {
            scheduler: spin::Once::new(),
        }
    }

    /// Pick the next task to run from scheduler.
    /// Require the scheduler is not none.
    pub fn pick_next_task(&self) -> Option<SchedTask> {
        self.scheduler.get().and_then(|s| s.pick_next_task())
    }

    /// Enqueue a task into scheduler.
    /// Require the scheduler is not none.
    pub fn enqueue(&self, task: SchedTask) {
        self.scheduler.get().unwrap().enqueue(task);
    }

    /// Remove the task and its related information from the scheduler.
    pub fn clear(&self, task: &Task) {
        self.scheduler.get().unwrap().clear(task);
    }

    pub fn should_preempt_cur_task(&self) -> bool {
        self.scheduler.get().unwrap().should_preempt_cur_task()
    }

    pub fn tick_cur_task(&self) {
        self.scheduler.get().unwrap().tick_cur_task();
    }

    pub fn prepare_to_yield_cur_task(&self) {
        self.scheduler.get().unwrap().prepare_to_yield_cur_task()
    }

    pub fn prepare_to_yield_to(&self, task: &SchedTask) {
        self.scheduler.get().unwrap().prepare_to_yield_to(task)
    }
}

/// Set the global task scheduler.
///
/// This must be called before invoking `Task::spawn`.
pub fn set_scheduler(scheduler: &'static dyn Scheduler) {
    GLOBAL_SCHEDULER.scheduler.call_once(|| scheduler);
}

/// Pick the next task to run from scheduler.
/// The scheduler will pick the most appropriate task eligible to run next if any.
pub(crate) fn pick_next_task() -> Option<SchedTask> {
    assert!(is_in_preemption());
    let task = GLOBAL_SCHEDULER.pick_next_task();
    if let Some(t) = &task {
        assert!(!t.is_linked());
    }
    match &task {
        Some(task) => sched_debug!("fetch next task: {:p}", task.as_ptr()),
        None => sched_debug!("fetch next task: None"),
    }
    task
}

/// Enqueue a task into scheduler.
#[track_caller]
pub fn add_task(task: Arc<Task>) {
    assert!(task.status().is_runnable());
    sched_debug!("add task: {:p}", Arc::as_ptr(&task));
    let _guard = DisablePreemptGuard::new();
    GLOBAL_SCHEDULER.enqueue(SchedTask::new(task));
    // GLOBAL_SCHEDULER.scheduler.get().unwrap().traverse();
}

/// Remove all the information of the task from the scheduler.
pub fn clear_task(task: &Task) {
    let _guard = DisablePreemptGuard::new();
    GLOBAL_SCHEDULER.clear(task);
}

pub fn trigger_load_balancing() {
    if let Some(sched) = GLOBAL_SCHEDULER.scheduler.get() {
        let _guard = DisablePreemptGuard::new();
        sched.load_balance()
    }
}
