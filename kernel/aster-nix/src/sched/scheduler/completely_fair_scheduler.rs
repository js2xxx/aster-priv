// SPDX-License-Identifier: MPL-2.0
#![allow(unsafe_code)]
#![warn(unused)]

use alloc::sync::Arc;
use core::{
    cmp::{max_by_key, Ordering},
    sync::atomic::{AtomicU64, Ordering::*},
};

use aster_frame::{
    arch::read_tsc,
    cpu::{num_cpus, this_cpu},
    task::{
        with_current, NeedResched, ReadPriority, SchedTask, Scheduler, Task, TaskAdapter,
        TaskStatus,
    },
    trap::{disable_local, is_local_enabled},
};
use intrusive_collections::{intrusive_adapter, KeyAdapter, LinkedList, RBTree, RBTreeAtomicLink};

use crate::{prelude::*, sched::nice::Nice};

pub const fn nice_to_weight(nice: Nice) -> u64 {
    const NICE_TO_WEIGHT: [u64; 40] = [
        88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100,
        4904, 3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172,
        137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
    ];
    NICE_TO_WEIGHT[(nice.to_raw() + 20) as usize]
}
const WEIGHT_0: u64 = nice_to_weight(Nice::new(0));

/// The virtual runtime
#[derive(Clone, Copy)]
pub struct VRuntime {
    key: usize,
    vruntime: u64,
    start: u64,
    weight: u64,
}

impl VRuntime {
    pub fn new(task: &Task) -> VRuntime {
        let nice = Nice::new(task.priority().as_nice().unwrap());
        VRuntime {
            key: task as *const Task as usize,
            vruntime: 0,
            start: read_tsc(),
            weight: nice_to_weight(nice),
        }
    }

    fn get_with_cur(&self, cur: u64) -> u64 {
        self.vruntime + ((cur - self.start) * WEIGHT_0 / self.weight)
    }

    pub fn get(&self) -> u64 {
        self.vruntime
    }

    pub fn update(&mut self) {
        let cur = read_tsc();
        self.vruntime = self.get_with_cur(cur);
        self.start = cur;
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

intrusive_adapter!(VrAdapter = Arc<Task>: Task { link: RBTreeAtomicLink });
impl<'a> KeyAdapter<'a> for VrAdapter {
    type Key = VRuntime;

    fn get_key(&self, value: &'a Task) -> VRuntime {
        *value.sched_entity.lock().get().unwrap()
    }
}

fn vr(task: &Task) -> &VRuntime {
    // SAFETY: task is contained in the RB tree of our current scheduler.
    unsafe { (*task.sched_entity.as_ptr()).get::<VRuntime>().unwrap() }
}

pub struct RunQueue {
    cpu: u32,

    /// Tasks with a priority of less than 100 are regarded as real-time tasks.
    real_time_tasks: SpinLock<LinkedList<TaskAdapter>>,

    min_vruntime: AtomicU64,
    /// Tasks with a priority greater than or equal to 100 are regarded as normal tasks.
    normal_tasks: SpinLock<RBTree<VrAdapter>>,

    load: AtomicU64,
}

impl RunQueue {
    pub fn new(cpu: u32) -> Self {
        Self {
            cpu,
            real_time_tasks: SpinLock::new(LinkedList::new(Default::default())),

            min_vruntime: AtomicU64::new(u64::MAX / 2),
            normal_tasks: SpinLock::new(RBTree::new(VrAdapter::new())),

            load: AtomicU64::new(0),
        }
    }

    #[track_caller]
    fn push(&self, task: Arc<Task>) {
        if task.is_real_time() {
            self.real_time_tasks
                .lock_irq_disabled()
                .push_back(task.clone());
        } else {
            assert_eq!(task.status(), TaskStatus::Runnable, "{task:p}");
            task.transition(|status| {
                assert_eq!(*status, TaskStatus::Runnable, "{task:p}");
                *status = TaskStatus::Ready(self.cpu);
            });

            task.set_need_resched(false);
            let _irq = disable_local();

            {
                let mut se = task.sched_entity.lock();
                let ent = se.entry();
                let vruntime = ent.or_insert_with(|| VRuntime::new(&task));

                self.load.fetch_add(vruntime.weight, Relaxed);
                // if !force_yield {
                //     let min_boundary = (self.min_vruntime.load(Relaxed) - BANDWIDTH).max(0);
                //     let delta = (min_boundary - vruntime.vruntime).max(1);
                //     vruntime.vruntime += delta.ilog2() as u64
                // }
            }

            let mut map = self.normal_tasks.lock();
            map.insert(task);
            let min_vruntime = vr(map.front().get().unwrap()).get();
            self.min_vruntime.store(min_vruntime, Relaxed);
        }
    }

    fn pop(&self, target_cpu: u32) -> Option<Arc<Task>> {
        assert!(!is_local_enabled());

        let mut real_time_tasks = self.real_time_tasks.lock();
        if !real_time_tasks.is_empty() {
            return real_time_tasks.pop_front();
        }
        drop(real_time_tasks);

        let mut set = self.normal_tasks.lock();
        let mut front = set.front_mut();
        let (task, min) = loop {
            let task = front.get()?;

            if !task.status().is_ready(self.cpu) {
                self.load.fetch_sub(vr(task).weight, Relaxed);
                println!("dropping {task:p}: {:?}", task.status());
                drop(front.remove());
                continue;
            }
            if !task.cpu_affinity.contains(target_cpu) {
                front.move_next();
                continue;
            }

            let task = front.remove().unwrap();
            let min = match front.get() {
                Some(task) => vr(task).get(),
                None => u64::MAX / 2,
            };
            self.load.fetch_sub(vr(&task).weight, Relaxed);
            break (task, min);
        };
        // SAFETY: task is contained in the RB tree of our current scheduler.
        if let Some(vr) = unsafe { (*task.sched_entity.as_ptr()).get_mut::<VRuntime>() } {
            vr.start = read_tsc();
        }
        task.transition(|status| {
            assert_eq!(*status, TaskStatus::Ready(self.cpu));
            *status = TaskStatus::Runnable;
        });

        self.min_vruntime.store(min, Relaxed);
        assert!(!task.is_linked());
        Some(task)
    }

    fn should_preempt(&self) -> bool {
        with_current(|cur| {
            assert!(!is_local_enabled());
            let se = cur.sched_entity.lock();
            match se.get::<VRuntime>() {
                Some(vr) => vr.get() > self.min_vruntime.load(Relaxed),
                None => true,
            }
        })
        .unwrap_or(true)
    }

    fn tick(&self) {
        with_current(|cur| {
            assert!(!is_local_enabled());
            let mut se = cur.sched_entity.lock();
            if let Some(v) = se.get_mut::<VRuntime>() {
                v.tick();

                if v.vruntime > self.min_vruntime.load(Relaxed) {
                    cur.set_need_resched(true);
                }
            }
        });
    }
}

/// The Completely Fair Scheduler(CFS)
///
/// Real-time tasks are placed in the `real_time_tasks` queue and
/// are always prioritized during scheduling.
/// Normal tasks are placed in the `normal_tasks` queue and are only
/// scheduled for execution when there are no real-time tasks.
pub struct CompletelyFairScheduler {
    rq: Vec<RunQueue>,
}

impl CompletelyFairScheduler {
    pub fn new() -> Self {
        CompletelyFairScheduler {
            rq: (0..num_cpus()).map(RunQueue::new).collect(),
        }
    }

    fn cur_rq(&self) -> &RunQueue {
        &self.rq[this_cpu() as usize]
    }

    #[track_caller]
    fn push(&self, task: Arc<Task>) {
        if task.cpu_affinity.contains(this_cpu()) {
            self.cur_rq()
        } else {
            let cpu = task.cpu_affinity.iter().next().expect("empty affinity");
            &self.rq[cpu]
        }
        .push(task)
    }
}

impl Default for CompletelyFairScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for CompletelyFairScheduler {
    fn enqueue(&self, task: SchedTask) {
        self.push(unsafe { task.into_raw() })
    }

    fn pick_next_task(&self) -> Option<SchedTask> {
        (self.cur_rq().pop(this_cpu())).map(|task| unsafe { SchedTask::from_raw(task) })
    }

    fn clear(&self, task: &Task) {
        task.sched_entity.lock_irq_disabled().remove::<VRuntime>();
    }

    fn should_preempt_cur_task(&self) -> bool {
        self.cur_rq().should_preempt()
    }

    fn tick_cur_task(&self) {
        self.cur_rq().tick();

        PACE.with(|pace| {
            let cur_tick = aster_frame::arch::current_tick();
            if cur_tick >= pace.get() + 5000 {
                pace.set(cur_tick);
                // self.traverse();
            }
        });

        aster_frame::cpu_local! {
            static PACE: core::cell::Cell<u64> = core::cell::Cell::new(0);
        }
    }

    fn prepare_to_yield_cur_task(&self) {
        self.tick_cur_task()
    }

    fn prepare_to_yield_to(&self, task: &SchedTask) {
        let mut se = task.sched_entity.lock_irq_disabled();
        se.entry().or_insert_with(|| VRuntime::new(task));
    }

    fn load_balance(&self) {
        if let Some(src) = (self.rq.iter())
            .reduce(|a, b| max_by_key(a, b, |t| t.load.load(Relaxed)))
            .filter(|rq| rq.load.load(Relaxed) > 0)
        {
            let target = self.cur_rq();
            if src.cpu != target.cpu {
                while src.load.load(Relaxed) > target.load.load(Relaxed) + WEIGHT_0 {
                    let Some(task) = src.pop(target.cpu) else {
                        break;
                    };
                    assert!(task.status().is_runnable());
                    atomic::fence(SeqCst);
                    // println!("requeueing {task:p} from {} to {}", src.cpu, target.cpu);
                    target.push(task);
                }
            }
        }
    }

    fn traverse(&self) {
        with_current(|cur| {
            let cur_tick = aster_frame::arch::current_tick();
            println!(
                "CPU#{} cur_tick = {cur_tick}, cur_task = {:p}({}), min = {}",
                this_cpu(),
                cur.as_ptr(),
                vr(cur).get(),
                self.cur_rq().min_vruntime.load(Relaxed)
            );
            print!("num_ready = ");
            for r in &*self.cur_rq().normal_tasks.lock() {
                print!("{:p}({}), ", r, vr(r).get());
            }
            println!("\n");
        });
    }
}
