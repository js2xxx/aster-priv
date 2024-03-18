// SPDX-License-Identifier: MPL-2.0

use alloc::{
    collections::{BTreeMap, BinaryHeap},
    sync::Arc,
};
use core::{
    cmp::Ordering,
    sync::atomic::{AtomicIsize, Ordering::*},
};

use aster_frame::task::{set_scheduler, ReadPriority, Scheduler, Task, TaskAdapter};
use intrusive_collections::LinkedList;

use crate::{prelude::*, sched::nice::Nice};

pub fn init() {
    let completely_fair_scheduler = Box::new(CompletelyFairScheduler::new());
    let scheduler = Box::<CompletelyFairScheduler>::leak(completely_fair_scheduler);
    set_scheduler(scheduler);
}

pub fn nice_to_weight(nice: Nice) -> isize {
    const NICE_TO_WEIGHT: [isize; 40] = [
        88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100,
        4904, 3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172,
        137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
    ];
    NICE_TO_WEIGHT[(nice.to_raw() + 20) as usize]
}

/// The virtual runtime
#[derive(Clone)]
pub struct VRuntime {
    vruntime: isize,
    delta: isize,
    nice: Nice,

    task: Arc<Task>,
}

impl VRuntime {
    pub fn new(scheduler: &CompletelyFairScheduler, task: Arc<Task>) -> VRuntime {
        VRuntime {
            // BUG: Keeping creating new tasks can cause starvation.
            vruntime: scheduler.min_vruntime.load(SeqCst),
            delta: 0,
            nice: Nice::new(task.priority().as_nice().unwrap()),
            task,
        }
    }

    pub fn weight(&self) -> isize {
        nice_to_weight(self.nice)
    }

    pub fn get(&self) -> isize {
        self.vruntime + (self.delta * 1024 / self.weight())
    }

    pub fn set(&mut self, vruntime: isize) {
        self.vruntime = vruntime;
    }

    pub fn set_nice(&mut self, nice: Nice) {
        self.set(self.get());
        self.delta = 0;
        self.nice = nice;
    }

    pub fn tick(&mut self) {
        self.delta += 1;
        self.set(self.get());
    }
}

impl Ord for VRuntime {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse the result for the implementation of min-heap.
        other.get().cmp(&self.get())
    }
}

impl PartialOrd for VRuntime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for VRuntime {}

impl PartialEq for VRuntime {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

/// The Completely Fair Scheduler(CFS)
///
/// Real-time tasks are placed in the `real_time_tasks` queue and
/// are always prioritized during scheduling.
/// Normal tasks are placed in the `normal_tasks` queue and are only
/// scheduled for execution when there are no real-time tasks.
pub struct CompletelyFairScheduler {
    /// Tasks with a priority of less than 100 are regarded as real-time tasks.
    real_time_tasks: SpinLock<LinkedList<TaskAdapter>>,

    min_vruntime: AtomicIsize,
    // TODO: `vruntimes` currently never shrinks.
    /// `VRuntime`'s created are stored here for looking up.
    vruntimes: SpinLock<BTreeMap<usize, VRuntime>>,
    /// Tasks with a priority greater than or equal to 100 are regarded as normal tasks.
    normal_tasks: SpinLock<BinaryHeap<VRuntime>>,
}

impl CompletelyFairScheduler {
    pub fn new() -> Self {
        Self {
            real_time_tasks: SpinLock::new(LinkedList::new(Default::default())),

            min_vruntime: AtomicIsize::new(0),
            vruntimes: SpinLock::new(BTreeMap::new()),
            normal_tasks: SpinLock::new(BinaryHeap::new()),
        }
    }
}

impl Default for CompletelyFairScheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn key_of(task: &Arc<Task>) -> usize {
    Arc::as_ptr(task) as usize
}

impl Scheduler for CompletelyFairScheduler {
    fn enqueue(&self, task: Arc<Task>) {
        if task.is_real_time() {
            self.real_time_tasks
                .lock_irq_disabled()
                .push_back(task.clone());
        } else {
            // BUG: address is not a strictly unique key
            let key = key_of(&task);

            let vruntime = self
                .vruntimes
                .lock_irq_disabled()
                .remove(&key)
                .unwrap_or_else(|| VRuntime::new(self, task));
            let mut heap = self.normal_tasks.lock_irq_disabled();
            heap.push(vruntime);
            self.min_vruntime.store(heap.peek().unwrap().get(), SeqCst);
        }
    }

    fn pick_next_task(&self) -> Option<Arc<Task>> {
        let mut real_time_tasks = self.real_time_tasks.lock_irq_disabled();
        if !real_time_tasks.is_empty() {
            return real_time_tasks.pop_front();
        }
        drop(real_time_tasks);

        let vruntime = {
            let mut heap = self.normal_tasks.lock_irq_disabled();
            let ret = heap.pop()?;
            if let Some(peek) = heap.peek() {
                self.min_vruntime.store(peek.get(), SeqCst);
            }
            ret
        };
        let task = vruntime.task.clone();
        let key = key_of(&task);
        self.vruntimes.lock_irq_disabled().insert(key, vruntime);
        Some(task)
    }

    fn clear(&self, task: &Arc<Task>) {
        self.vruntimes.lock_irq_disabled().remove(&key_of(task));
    }

    fn should_preempt_cur_task(&self) -> bool {
        let mut vruntimes = self.vruntimes.lock_irq_disabled();
        if let Some(mut cur) = vruntimes.first_entry() {
            return cur.get_mut().get() > self.min_vruntime.load(SeqCst);
        }
        false
    }

    fn tick_cur_task(&self) {
        let mut vruntimes = self.vruntimes.lock_irq_disabled();
        if let Some(mut cur) = vruntimes.first_entry() {
            cur.get_mut().tick()
        }
    }

    fn prepare_to_yield_to(&self, task: Arc<Task>) {
        let _ = task;
    }
}
