// SPDX-License-Identifier: MPL-2.0

//! Tasks are the unit of code execution.

mod preempt;
mod priority;
mod processor;
mod scheduler;
#[allow(clippy::module_inception)]
mod task;

pub use self::{
    preempt::{in_atomic, is_preemptible, DisablePreemptGuard},
    priority::Priority,
    processor::{current_task, schedule, with_current, yield_now, yield_to},
    scheduler::{add_task, clear_task, set_scheduler, trigger_load_balancing, Scheduler},
    task::{
        NeedResched, ReadPriority, SchedTask, Task, TaskAdapter, TaskOptions, TaskStatus, WakeUp,
    },
};

pub fn init() {
    self::processor::init();
}
