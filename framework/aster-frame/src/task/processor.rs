// SPDX-License-Identifier: MPL-2.0

use alloc::sync::Arc;
use core::cell::RefCell;

use super::{
    preempt::{
        activate_preemption, deactivate_preemption, in_atomic, panic_if_in_atomic,
        panic_if_not_preemptible,
    },
    scheduler::{add_task, pick_next_task, GLOBAL_SCHEDULER},
    task::{context_switch, NeedResched, Task, TaskContext},
};
use crate::{arch::timer::register_scheduler_tick, cpu_local};

#[derive(Default)]
pub struct Processor {
    current: Option<Arc<Task>>,
    idle_task_ctx: TaskContext,
}

impl Processor {
    pub const fn new() -> Self {
        Self {
            current: None,
            idle_task_ctx: TaskContext::empty(),
        }
    }
    fn idle_task_ctx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_ctx as *mut _
    }
    pub fn current(&self) -> Option<&Arc<Task>> {
        self.current.as_ref()
    }
    pub fn set_current_task(&mut self, task: Arc<Task>) {
        self.current = Some(task);
    }
}

cpu_local! {
    static PROCESSOR: RefCell<Processor> = RefCell::new(Processor::new());
}

pub fn init() {
    register_scheduler_tick(scheduler_tick);
}

pub fn current_task() -> Option<Arc<Task>> {
    PROCESSOR.with_borrow(|processor| processor.current().cloned())
}

pub fn with_current<T>(f: impl FnOnce(&Arc<Task>) -> T) -> Option<T> {
    PROCESSOR.with_borrow(|processor| processor.current().map(f))
}

/// Yields execution so that another task may be scheduled.
/// Unlike in Linux, this will not change the task's status into runnable.
///
/// Note that this method cannot be simply named "yield" as the name is
/// a Rust keyword.
pub fn yield_now() -> bool {
    if with_current(|_| {}).is_some() {
        GLOBAL_SCHEDULER.prepare_to_yield_cur_task();
    }
    schedule()
}

// FIXME: remove this func after merging #632.
pub fn yield_to(task: Arc<Task>) {
    panic_if_not_preemptible();

    deactivate_preemption();
    switch_to(task);
    activate_preemption();

    panic_if_not_preemptible();
}

/// Switch to the next task selected by the global scheduler if it should.
pub fn schedule() -> bool {
    panic_if_not_preemptible();

    deactivate_preemption();
    let mut ret = should_preempt_cur_task();
    if ret {
        match pick_next_task() {
            None => ret = false,
            Some(next_task) => switch_to(next_task),
        }
    }
    activate_preemption();

    panic_if_not_preemptible();
    ret
}

fn should_preempt_cur_task() -> bool {
    if in_atomic() {
        return false;
    }

    with_current(|cur_task| !cur_task.status().is_runnable() || cur_task.need_resched())
        .unwrap_or(true)
        || GLOBAL_SCHEDULER.should_preempt_cur_task()
}

/// Switch to the given next task.
/// - If current task is none, then it will use the default task context
/// and it will not return to this function again.
/// - If current task status is exit, then it will not add to the scheduler.
///
/// After context switch, the current task of the processor
/// will be switched to the given next task.
///
/// This method should be called with preemption guard.
fn switch_to(next_task: Arc<Task>) {
    panic_if_in_atomic();
    let next_task_ctx = next_task.context();

    let (current_task_ctx, cur_task) = PROCESSOR.with_borrow_mut(|processor| {
        let cur_task = processor.current.replace(next_task);

        let ctx = match &cur_task {
            None => processor.idle_task_ctx_ptr(),
            Some(cur_task) => &mut cur_task.inner_exclusive_access().ctx as _,
        };
        (ctx, cur_task)
    });
    if let Some(cur_task) = cur_task {
        if cur_task.status().is_runnable() {
            add_task(cur_task);
        }
    }
    unsafe {
        context_switch(current_task_ctx, &next_task_ctx);
    }
}

/// Called by the timer handler at every TICK update.
fn scheduler_tick() {
    if with_current(|_| {}).is_some() {
        GLOBAL_SCHEDULER.tick_cur_task();
    }
}
