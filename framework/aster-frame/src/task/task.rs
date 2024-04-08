// SPDX-License-Identifier: MPL-2.0

use core::{
    ops::Deref,
    sync::atomic::{AtomicBool, Ordering::*},
};

use intrusive_collections::{intrusive_adapter, RBTreeAtomicLink};

use super::{
    add_task, clear_task,
    preempt::{activate_preemption, panic_if_in_atomic},
    priority::Priority,
    processor::{current_task, schedule},
    yield_to,
};
use crate::{
    arch::mm::PageTableFlags,
    config::{KERNEL_STACK_SIZE, PAGE_SIZE},
    cpu::CpuSet,
    prelude::*,
    sync::{SpinLock, SpinLockGuard},
    timer::current_tick,
    user::UserSpace,
    vm::{page_table::KERNEL_PAGE_TABLE, VmAllocOptions, VmSegment},
};

type SchedEntityMap = anymap3::hashbrown::Map<dyn Any + Send + Sync>;

core::arch::global_asm!(include_str!("switch.S"));

#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct CalleeRegs {
    pub rsp: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

impl CalleeRegs {
    pub const fn zeros() -> Self {
        Self {
            rsp: 0,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub(crate) struct TaskContext {
    pub regs: CalleeRegs,
    pub rip: usize,
}

impl TaskContext {
    pub const fn empty() -> Self {
        Self {
            regs: CalleeRegs::zeros(),
            rip: 0,
        }
    }
}

extern "C" {
    pub(crate) fn context_switch(cur: *mut TaskContext, nxt: *const TaskContext);
}

pub struct KernelStack {
    segment: VmSegment,
    old_guard_page_flag: Option<PageTableFlags>,
}

impl KernelStack {
    pub fn new() -> Result<Self> {
        Ok(Self {
            segment: VmAllocOptions::new(KERNEL_STACK_SIZE / PAGE_SIZE)
                .is_contiguous(true)
                .alloc_contiguous()?,
            old_guard_page_flag: None,
        })
    }

    /// Generate a kernel stack with a guard page.
    /// An additional page is allocated and be regarded as a guard page, which should not be accessed.  
    pub fn new_with_guard_page() -> Result<Self> {
        let stack_segment = VmAllocOptions::new(KERNEL_STACK_SIZE / PAGE_SIZE + 1)
            .is_contiguous(true)
            .alloc_contiguous()?;
        let unpresent_flag = PageTableFlags::empty();
        let old_guard_page_flag = Self::protect_guard_page(&stack_segment, unpresent_flag);
        Ok(Self {
            segment: stack_segment,
            old_guard_page_flag: Some(old_guard_page_flag),
        })
    }

    pub fn end_paddr(&self) -> Paddr {
        self.segment.end_paddr()
    }

    pub fn has_guard_page(&self) -> bool {
        self.old_guard_page_flag.is_some()
    }

    fn protect_guard_page(stack_segment: &VmSegment, flags: PageTableFlags) -> PageTableFlags {
        let mut kernel_pt = KERNEL_PAGE_TABLE.get().unwrap().lock_irq_disabled();
        let guard_page_vaddr = {
            let guard_page_paddr = stack_segment.start_paddr();
            crate::vm::paddr_to_vaddr(guard_page_paddr)
        };
        // Safety: The protected address must be the address of guard page hence it should be safe and valid.
        unsafe { kernel_pt.protect(guard_page_vaddr, flags).unwrap() }
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        if self.has_guard_page() {
            Self::protect_guard_page(&self.segment, self.old_guard_page_flag.unwrap());
        }
    }
}

/// A task that executes a function to the end.
pub struct Task {
    func: Box<dyn Fn() + Send + Sync>,
    data: Box<dyn Any + Send + Sync>,
    user_space: Option<Arc<UserSpace>>,
    task_inner: SpinLock<TaskInner>,
    exit_code: usize,
    /// kernel stack, note that the top is SyscallFrame/TrapFrame
    kstack: KernelStack,
    need_resched: AtomicBool,
    priority: Priority,
    // TODO:: add multiprocessor support
    pub cpu_affinity: CpuSet,
    pub sched_entity: SpinLock<SchedEntityMap>,
    pub link: RBTreeAtomicLink,
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self, other)
    }
}

// TaskAdapter struct is implemented for building relationships between doubly linked list and Task struct
intrusive_adapter!(pub TaskAdapter = Arc<Task>: Task { link: RBTreeAtomicLink });

pub(crate) struct TaskInner {
    task_status: TaskStatus,
    pub ctx: TaskContext,
    pub woken_up_timestamp: Option<u64>, // in Tick
}

impl Task {
    /// Guarded access to the task inner.
    pub(crate) fn inner_exclusive_access(&self) -> SpinLockGuard<'_, TaskInner> {
        self.task_inner.lock_irq_disabled()
    }

    pub(crate) fn context(&self) -> TaskContext {
        self.task_inner.lock_irq_disabled().ctx
    }

    pub fn is_linked(&self) -> bool {
        self.link.is_linked()
    }

    pub fn run(self: &Arc<Self>) {
        debug_assert!(self.status().is_runnable());
        // FIXME: remove CHILD_RUN_FIRST after merging
        const CHILD_RUN_FIRST: bool = true;
        if CHILD_RUN_FIRST {
            yield_to(self.clone());
        } else {
            add_task(self.clone());
            schedule();
        }
    }

    /// Returns the task status.
    pub fn status(&self) -> TaskStatus {
        self.task_inner.lock_irq_disabled().task_status
    }

    #[track_caller]
    pub fn transition<T>(&self, f: impl FnOnce(&mut TaskStatus) -> T) -> T {
        f(&mut self.inner_exclusive_access().task_status)
    }

    /// Returns the task data.
    pub fn data(&self) -> &Box<dyn Any + Send + Sync> {
        &self.data
    }

    /// Returns the user space of this task, if it has.
    pub fn user_space(&self) -> Option<&Arc<UserSpace>> {
        if self.user_space.is_some() {
            Some(self.user_space.as_ref().unwrap())
        } else {
            None
        }
    }

    pub fn exit(&self) -> ! {
        self.inner_exclusive_access().task_status = TaskStatus::Exited;
        clear_task(self);
        schedule();
        panic_if_in_atomic();
        loop {
            super::yield_now();
            core::hint::spin_loop();
        }
    }
}

#[repr(transparent)]
pub struct SchedTask(Arc<Task>);

impl SchedTask {
    #[track_caller]
    pub fn new(task: Arc<Task>) -> SchedTask {
        // println!("+{task:p}:#{}", crate::cpu::this_cpu(),);
        SchedTask(task)
    }

    /// # Safety
    ///
    /// Must not be cloned implicitly.
    pub unsafe fn from_raw(task: Arc<Task>) -> SchedTask {
        SchedTask(task)
    }

    /// # Safety
    ///
    /// Must not be cloned implicitly.
    pub unsafe fn into_raw(self) -> Arc<Task> {
        let copy = core::ptr::read(&self.0);
        core::mem::forget(self);
        copy
    }

    pub fn as_ptr(&self) -> *const Task {
        Arc::as_ptr(&self.0)
    }

    pub(crate) fn clone_inner(&self) -> Arc<Task> {
        // println!("~{:p}", self.0);
        self.0.clone()
    }
}

impl Deref for SchedTask {
    type Target = Task;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for SchedTask {
    fn drop(&mut self) {
        // println!("-{:p}:#{}", self.0, crate::cpu::this_cpu());
    }
}

pub trait ReadPriority {
    fn priority(&self) -> Priority;

    fn is_real_time(&self) -> bool;
}
impl ReadPriority for Task {
    fn priority(&self) -> Priority {
        self.priority
    }

    fn is_real_time(&self) -> bool {
        self.priority.is_real_time()
    }
}

pub trait WakeUp {
    fn sleep(&self);

    fn wakeup(&self) -> bool;

    fn woken_up_timestamp(&self) -> Option<u64>;

    fn clear_woken_up_timestamp(&self);
}
impl WakeUp for Task {
    fn sleep(&self) {
        let mut inner = self.task_inner.lock_irq_disabled();
        inner.task_status = match inner.task_status {
            TaskStatus::Running(cpu) => TaskStatus::ReadyToSleep(cpu),
            s => panic!("{:?}, expected Running({})", s, crate::cpu::this_cpu()),
        };
    }

    fn wakeup(&self) -> bool {
        let mut inner = self.task_inner.lock_irq_disabled();
        match inner.task_status {
            TaskStatus::Sleeping => {
                inner.task_status = TaskStatus::Runnable;
                inner.woken_up_timestamp = Some(current_tick());
                true
            }
            TaskStatus::ReadyToSleep(cpu) => {
                inner.task_status = TaskStatus::Running(cpu);
                inner.woken_up_timestamp = Some(current_tick());
                false
            }
            _ => false,
        }
    }

    fn woken_up_timestamp(&self) -> Option<u64> {
        self.task_inner.lock().woken_up_timestamp
    }

    fn clear_woken_up_timestamp(&self) {
        self.task_inner.lock().woken_up_timestamp = None;
    }
}

/// To get or set the `need_resched` flag of a task.
pub trait NeedResched {
    fn need_resched(&self) -> bool;
    fn set_need_resched(&self, need_resched: bool);
}
impl NeedResched for Task {
    fn set_need_resched(&self, need_resched: bool) {
        self.need_resched.store(need_resched, Release);
    }

    fn need_resched(&self) -> bool {
        self.need_resched.load(Acquire)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
/// The status of a task.
pub enum TaskStatus {
    /// The task is runnable.
    Runnable,
    Ready(u32),
    Running(u32),
    /// The task is sleeping.
    Sleeping,
    ReadyToSleep(u32),
    /// The task has exited.
    Exited,
}

impl TaskStatus {
    pub fn is_runnable(&self) -> bool {
        self == &TaskStatus::Runnable
    }

    pub fn is_ready(&self, cpu: u32) -> bool {
        *self == TaskStatus::Ready(cpu)
    }

    pub fn is_sleeping(&self) -> bool {
        self == &TaskStatus::Sleeping
    }

    pub fn is_exited(&self) -> bool {
        self == &TaskStatus::Exited
    }
}

/// Options to create or spawn a new task.
pub struct TaskOptions {
    func: Option<Box<dyn Fn() + Send + Sync>>,
    data: Option<Box<dyn Any + Send + Sync>>,
    user_space: Option<Arc<UserSpace>>,
    priority: Priority,
    cpu_affinity: CpuSet,
}

impl TaskOptions {
    /// Creates a set of options for a task.
    pub fn new<F>(func: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let cpu_affinity = CpuSet::new_full();
        Self {
            func: Some(Box::new(func)),
            data: None,
            user_space: None,
            priority: Priority::normal(),
            cpu_affinity,
        }
    }

    pub fn func<F>(mut self, func: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.func = Some(Box::new(func));
        self
    }

    pub fn data<T>(mut self, data: T) -> Self
    where
        T: Any + Send + Sync,
    {
        self.data = Some(Box::new(data));
        self
    }

    /// Sets the user space associated with the task.
    pub fn user_space(mut self, user_space: Option<Arc<UserSpace>>) -> Self {
        self.user_space = user_space;
        self
    }

    /// Sets the priority of the task.
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn cpu_affinity(mut self, cpu_affinity: CpuSet) -> Self {
        self.cpu_affinity = cpu_affinity;
        self
    }

    /// Builds a new task but not run it immediately.
    pub fn build(self) -> Result<Arc<Task>> {
        let mut result = Task {
            func: self.func.unwrap(),
            data: self.data.unwrap(),
            user_space: self.user_space,
            task_inner: SpinLock::new(TaskInner {
                task_status: TaskStatus::Runnable,
                ctx: TaskContext::default(),
                woken_up_timestamp: None,
            }),
            exit_code: 0,
            kstack: KernelStack::new_with_guard_page()?,
            need_resched: false.into(),
            priority: self.priority,
            cpu_affinity: self.cpu_affinity,
            sched_entity: SpinLock::new(SchedEntityMap::new()),
            link: RBTreeAtomicLink::new(),
        };

        result.task_inner.get_mut().ctx.rip = kernel_task_entry as usize;
        result.task_inner.get_mut().ctx.regs.rsp =
            (crate::vm::paddr_to_vaddr(result.kstack.end_paddr())) as u64;

        Ok(Arc::new(result))
    }

    /// Builds a new task and run it immediately.
    ///
    /// Each task is associated with a per-task data and an optional user space.
    /// If having a user space, then the task can switch to the user space to
    /// execute user code. Multiple tasks can share a single user space.
    pub fn spawn(self) -> Result<Arc<Task>> {
        let mut result = Task {
            func: self.func.unwrap(),
            data: self.data.unwrap(),
            user_space: self.user_space,
            task_inner: SpinLock::new(TaskInner {
                task_status: TaskStatus::Runnable,
                ctx: TaskContext::default(),
                woken_up_timestamp: None,
            }),
            exit_code: 0,
            kstack: KernelStack::new_with_guard_page()?,
            need_resched: false.into(),
            priority: self.priority,
            cpu_affinity: self.cpu_affinity,
            sched_entity: SpinLock::new(SchedEntityMap::new()),
            link: RBTreeAtomicLink::new(),
        };

        result.task_inner.get_mut().ctx.rip = kernel_task_entry as usize;
        result.task_inner.get_mut().ctx.regs.rsp =
            (crate::vm::paddr_to_vaddr(result.kstack.end_paddr())) as u64;

        let arc_self = Arc::new(result);
        arc_self.run();
        Ok(arc_self)
    }
}

/// All tasks will enter this function.
/// This function is meant to execute the `func` in [`Task`].
fn kernel_task_entry() {
    assert!(!crate::arch::irq::is_local_enabled());
    activate_preemption();
    let current_task =
        current_task().expect("no current task, it should have current task in kernel task entry");
    (current_task.func)();
    current_task.exit();
}
