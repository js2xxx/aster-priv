// SPDX-License-Identifier: MPL-2.0

use crate::{
    prelude::*,
    task::{SchedTaskBase, Task},
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
pub trait Scheduler<T: ?Sized + SchedTaskBase = Task>: Sync + Send {
    /// Add the task to the scheduler when it enters a runnable state.
    fn enqueue(&self, task: Arc<T>);

    /// Pick the most appropriate task eligible to run next from the scheduler.
    fn pick_next_task(&self) -> Option<Arc<T>>;

    /// Remove the task-related from the scheduler when the task is no longer alive.
    fn clear(&self, task: &Arc<T>);

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
        let cur_task = T::current();
        cur_task.set_need_resched(true);
        sched_debug!("before yield: {:p}", Arc::as_ptr(&cur_task));
    }

    // FIXME: remove this after merging #632.
    /// Yield the current task to the given task at best effort.
    fn prepare_to_yield_to(&self, task: Arc<T>);

    fn load_balance(&self) {}
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
    pub fn pick_next_task(&self) -> Option<Arc<Task>> {
        self.scheduler.get().and_then(|s| s.pick_next_task())
    }

    /// Enqueue a task into scheduler.
    /// Require the scheduler is not none.
    pub fn enqueue(&self, task: Arc<Task>) {
        self.scheduler.get().unwrap().enqueue(task)
    }

    /// Remove the task and its related information from the scheduler.
    pub fn clear(&self, task: &Arc<Task>) {
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

    // FIXME: remove this after merging #632.
    pub fn prepare_to_yield_to(&self, target_task: Arc<Task>) {
        self.scheduler
            .get()
            .unwrap()
            .prepare_to_yield_to(target_task)
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
pub fn pick_next_task() -> Option<Arc<Task>> {
    let task = GLOBAL_SCHEDULER.pick_next_task();
    match &task {
        Some(task) => sched_debug!("fetch next task: {:p}", Arc::as_ptr(task)),
        None => sched_debug!("fetch next task: None"),
    }
    task
}

/// Enqueue a task into scheduler.
pub fn add_task(task: Arc<Task>) {
    GLOBAL_SCHEDULER.enqueue(task.clone());
    sched_debug!("add task: {:p}", Arc::as_ptr(&task));
}

/// Remove all the information of the task from the scheduler.
pub fn clear_task(task: &Arc<Task>) {
    GLOBAL_SCHEDULER.clear(task);
    sched_debug!("remove task: {:p}", Arc::as_ptr(task));
}

pub fn trigger_load_balancing() {
    if let Some(sched) = GLOBAL_SCHEDULER.scheduler.get() {
        sched.load_balance()
    }
}
