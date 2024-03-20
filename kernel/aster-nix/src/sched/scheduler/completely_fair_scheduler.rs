// SPDX-License-Identifier: MPL-2.0
#![allow(unsafe_code)]

use alloc::sync::Arc;
use core::{
    cmp::Ordering,
    sync::atomic::{AtomicIsize, Ordering::*},
};

use aster_frame::{
    arch::raw_ticks,
    task::{with_current, NeedResched, ReadPriority, Scheduler, Task, TaskAdapter},
    trap::{disable_local, is_local_enabled},
};
use intrusive_collections::{intrusive_adapter, KeyAdapter, LinkedList, RBTree, RBTreeAtomicLink};

use crate::{prelude::*, sched::nice::Nice};

pub const fn nice_to_weight(nice: Nice) -> isize {
    const NICE_TO_WEIGHT: [isize; 40] = [
        88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100,
        4904, 3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172,
        137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
    ];
    NICE_TO_WEIGHT[(nice.to_raw() + 20) as usize]
}
const WEIGHT_0: isize = nice_to_weight(Nice::new(0));

/// The virtual runtime
#[derive(Clone, Copy)]
pub struct VRuntime {
    key: usize,
    vruntime: isize,
    start: u64,
    weight: isize,
}

impl VRuntime {
    pub fn new(task: &Task) -> VRuntime {
        let nice = Nice::new(task.priority().as_nice().unwrap() + 20);
        VRuntime {
            key: task as *const Task as usize,
            vruntime: 0,
            start: raw_ticks(),
            weight: nice_to_weight(nice),
        }
    }

    fn get_with_cur(&self, cur: u64) -> isize {
        self.vruntime + (((cur - self.start) as isize) * WEIGHT_0 / self.weight)
    }

    pub fn get(&self) -> isize {
        self.get_with_cur(self.start)
    }

    pub fn update(&mut self) {
        let cur = raw_ticks();
        self.vruntime = self.get_with_cur(cur);
        self.start = cur;
    }

    pub fn set_nice(&mut self, nice: Nice) {
        self.update();
        self.weight = nice_to_weight(nice);
    }

    pub fn tick(&mut self) {
        self.update();
    }
}

impl Ord for VRuntime {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.get().cmp(&other.get()))
            .then_with(|| self.start.cmp(&other.start))
            .then_with(|| self.key.cmp(&other.key))
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
        self.get() == other.get() && self.start == other.start && self.key == other.key
    }
}

intrusive_adapter!(VrAdapter = Arc<Task>: Task { rb_tree_link: RBTreeAtomicLink });
impl<'a> KeyAdapter<'a> for VrAdapter {
    type Key = VRuntime;

    fn get_key(&self, value: &'a Task) -> VRuntime {
        *value.sched_entity.lock().get().unwrap()
    }
}

fn vr(task: &Task) -> isize {
    // SAFETY: task is contained in the RB tree of our current scheduler.
    unsafe {
        (*task.sched_entity.as_ptr())
            .get::<VRuntime>()
            .unwrap()
            .get()
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
    /// Tasks with a priority greater than or equal to 100 are regarded as normal tasks.
    normal_tasks: SpinLock<RBTree<VrAdapter>>,
}

impl CompletelyFairScheduler {
    pub fn new() -> Self {
        Self {
            real_time_tasks: SpinLock::new(LinkedList::new(Default::default())),

            min_vruntime: AtomicIsize::new(0),
            normal_tasks: SpinLock::new(RBTree::new(VrAdapter::new())),
        }
    }

    fn push(&self, task: Arc<Task>, _force_yield: bool) {
        if task.is_real_time() {
            self.real_time_tasks
                .lock_irq_disabled()
                .push_back(task.clone());
        } else {
            task.set_need_resched(false);
            let _irq = disable_local();

            {
                let mut se = task.sched_entity.lock();
                let ent = se.entry();
                let _vruntime = ent.or_insert_with(|| VRuntime::new(&task));
                // if !force_yield {
                //     let min_boundary = (self.min_vruntime.load(Relaxed) - BANDWIDTH).max(0);
                //     let delta = (min_boundary - vruntime.vruntime).max(1);
                //     vruntime.vruntime += delta.ilog2() as isize
                // }
            }

            let mut map = self.normal_tasks.lock();
            map.insert(task);
            self.min_vruntime
                .store(vr(map.front().get().unwrap()), Relaxed);
        }
    }
}

impl Default for CompletelyFairScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for CompletelyFairScheduler {
    fn enqueue(&self, task: Arc<Task>) {
        self.push(task, false)
    }

    fn pick_next_task(&self) -> Option<Arc<Task>> {
        debug_assert!(!is_local_enabled());

        let mut real_time_tasks = self.real_time_tasks.lock();
        if !real_time_tasks.is_empty() {
            return real_time_tasks.pop_front();
        }
        drop(real_time_tasks);

        let mut set = self.normal_tasks.lock();
        let mut front = set.front_mut();
        let task = front.remove()?;
        let min = match front.get() {
            Some(task) => vr(task),
            None => 0,
        };
        self.min_vruntime.store(min, Relaxed);
        Some(task)
    }

    fn clear(&self, task: &Arc<Task>) {
        task.sched_entity.lock_irq_disabled().remove::<VRuntime>();
    }

    fn should_preempt_cur_task(&self) -> bool {
        with_current(|cur| {
            debug_assert!(!is_local_enabled());
            let se = cur.sched_entity.lock();
            match se.get::<VRuntime>() {
                Some(vr) => vr.get() > self.min_vruntime.load(Relaxed),
                None => true,
            }
        })
        .unwrap_or(true)
    }

    fn tick_cur_task(&self) {
        with_current(|cur| {
            debug_assert!(!is_local_enabled());
            let mut se = cur.sched_entity.lock();
            if let Some(vr) = se.get_mut::<VRuntime>() {
                vr.tick();

                if vr.vruntime > self.min_vruntime.load(Relaxed) {
                    cur.set_need_resched(true);
                }
                // let cur = vr.vruntime;
                // drop(vruntimes);

                // let cur_tick = raw_ticks();
                // if cur_tick > PACE.load(Relaxed) + 5000 {
                //     PACE.store(cur_tick, Relaxed);

                //     println!(
                //         "cur_tick = {cur_tick}, cur_task = {cur}, min = {}",
                //         self.min_vruntime.load(Relaxed)
                //     );
                //     print!("num_ready = ");
                //     for r in &*self.normal_tasks.lock() {
                //         print!("{}, ", r.vruntime);
                //     }
                //     print!("\nnum_waiting = ");
                //     for r in (*self.vruntimes.lock()).values() {
                //         print!("{}, ", r.vruntime);
                //     }
                //     println!();
                // }
            }
        });

        // static PACE: AtomicU64 = AtomicU64::new(0);
    }

    fn prepare_to_yield_cur_task(&self) {
        self.tick_cur_task()
    }

    fn prepare_to_yield_to(&self, task: Arc<Task>) {
        self.prepare_to_yield_cur_task();
        self.push(task, true)
    }
}
