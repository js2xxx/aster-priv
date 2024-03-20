// SPDX-License-Identifier: MPL-2.0

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use core::{
    cmp::Ordering,
    sync::atomic::{AtomicIsize, Ordering::*},
};

use aster_frame::{
    arch::current_tick,
    task::{current_task, NeedResched, ReadPriority, Scheduler, Task, TaskAdapter},
    trap::{disable_local, is_local_enabled},
};
use intrusive_collections::LinkedList;

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
#[derive(Clone)]
pub struct VRuntime {
    vruntime: isize,
    start: u64,
    weight: isize,

    task: Arc<Task>,
}

impl VRuntime {
    pub fn new(task: Arc<Task>) -> VRuntime {
        let nice = Nice::new(task.priority().as_nice().unwrap() + 20);
        VRuntime {
            vruntime: 0,
            start: current_tick(),
            weight: nice_to_weight(nice),
            task,
        }
    }

    fn get_with_cur(&self, cur: u64) -> isize {
        self.vruntime + (((cur - self.start) as isize) * WEIGHT_0 / self.weight)
    }

    pub fn get(&self) -> isize {
        self.get_with_cur(self.start)
    }

    pub fn update(&mut self) {
        let cur = current_tick();
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
        (self.get().cmp(&other.get())).then_with(|| self.start.cmp(&other.start))
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

const BANDWIDTH: isize = 500;

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
    /// `VRuntime`'s created are stored here for looking up.
    vruntimes: SpinLock<BTreeMap<usize, VRuntime>>,
    /// Tasks with a priority greater than or equal to 100 are regarded as normal tasks.
    normal_tasks: SpinLock<BTreeSet<VRuntime>>,
}

impl CompletelyFairScheduler {
    pub fn new() -> Self {
        Self {
            real_time_tasks: SpinLock::new(LinkedList::new(Default::default())),

            min_vruntime: AtomicIsize::new(0),
            vruntimes: SpinLock::new(BTreeMap::new()),
            normal_tasks: SpinLock::new(BTreeSet::new()),
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

            // BUG: address is not a strictly unique key
            let key = key_of(&task);

            let opt = self.vruntimes.lock().remove(&key);
            let vruntime = opt.unwrap_or_else(|| VRuntime::new(task));
            // if !force_yield {
            //     let min_boundary = (self.min_vruntime.load(Relaxed) - BANDWIDTH).max(0);
            //     let delta = (min_boundary - vruntime.vruntime).max(1);
            //     vruntime.vruntime += delta.ilog2() as isize
            // }

            let mut set = self.normal_tasks.lock();
            set.insert(vruntime);
            self.min_vruntime.store(set.first().unwrap().get(), Relaxed);
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
        self.push(task, false)
    }

    fn pick_next_task(&self) -> Option<Arc<Task>> {
        debug_assert!(!is_local_enabled());

        let mut real_time_tasks = self.real_time_tasks.lock();
        if !real_time_tasks.is_empty() {
            return real_time_tasks.pop_front();
        }
        drop(real_time_tasks);

        let vruntime = {
            let mut set = self.normal_tasks.lock();
            let ret = set.pop_first()?;
            let min = match set.first() {
                Some(peek) => peek.get(),
                None => 0,
            };
            self.min_vruntime.store(min, Relaxed);
            ret
        };
        let task = vruntime.task.clone();
        let key = key_of(&task);
        self.vruntimes.lock().insert(key, vruntime);
        Some(task)
    }

    fn clear(&self, task: &Arc<Task>) {
        self.vruntimes.lock_irq_disabled().remove(&key_of(task));
    }

    fn should_preempt_cur_task(&self) -> bool {
        if let Some(cur) = current_task() {
            if cur.is_real_time() {
                return true;
            }
            let key = key_of(&cur);

            debug_assert!(!is_local_enabled());
            return self.vruntimes.lock()[&key].get() > self.min_vruntime.load(Relaxed);
        }
        false
    }

    fn tick_cur_task(&self) {
        if let Some(cur) = current_task() {
            if cur.is_real_time() {
                return;
            }

            let key = key_of(&cur);

            debug_assert!(!is_local_enabled());
            let mut vruntimes = self.vruntimes.lock();
            let vruntime = vruntimes.get_mut(&key).unwrap();
            vruntime.tick();

            if vruntime.vruntime > self.min_vruntime.load(Relaxed) {
                cur.set_need_resched(true);
            }
            // let cur = vruntime.vruntime;
            // drop(vruntimes);

            // let cur_tick = current_tick();
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

        // static PACE: AtomicU64 = AtomicU64::new(0);
    }

    fn prepare_to_yield_to(&self, task: Arc<Task>) {
        self.prepare_to_yield_cur_task();
        self.push(task, true)
    }
}
