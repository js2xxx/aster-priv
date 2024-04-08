// SPDX-License-Identifier: MPL-2.0

#![warn(unused_variables)]

pub mod nice;
mod scheduler;

use alloc::boxed::Box;

use aster_frame::task::set_scheduler;
// use scheduler::fifo_with_rt_preempt::PreemptiveFIFOScheduler as Sched;
use scheduler::completely_fair_scheduler::CompletelyFairScheduler as Sched;

pub fn init() {
    let sched = Box::new(Sched::new());
    let sched = Box::<Sched>::leak(sched);
    set_scheduler(sched);
}
