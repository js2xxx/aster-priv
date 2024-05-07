// SPDX-License-Identifier: MPL-2.0

use core::{
    cell::Cell,
    marker::PhantomData,
    sync::atomic::{self, Ordering::*},
};

use crate::{
    arch::irq,
    cpu::this_cpu,
    trap::{disable_local, DisabledLocalIrqGuard},
};

#[thread_local]
static PREEMPT_INFO: PreemptInfo = PreemptInfo::new();

type Location = (u32, &'static core::panic::Location<'static>);
#[track_caller]
fn caller() -> Location {
    (this_cpu(), core::panic::Location::caller())
}

// TODO: unify the fields to avoid the inconsistency.
#[derive(Debug)]
struct PreemptInfo {
    /// The number of locks and irq-disabled contexts held by the current CPU.
    num: Cell<usize>,
    in_preemption: Cell<Option<Location>>,
}

impl PreemptInfo {
    const fn new() -> Self {
        Self {
            num: Cell::new(0),
            in_preemption: Cell::new(None),
        }
    }

    fn num(&self) -> usize {
        self.num.get()
    }

    fn in_preemption(&self) -> bool {
        self.in_preemption.get().is_some()
    }

    fn is_preemptible(&self) -> bool {
        let _local = disable_local();
        // TODO: may cause inconsistency
        !self.in_preemption() && !self.in_atomic()
    }

    /// Whether the current CPU is in atomic context,
    /// which means it holds some locks or is in IRQ context.
    fn in_atomic(&self) -> bool {
        self.num() != 0
    }
}

/// A guard to disable preempt.
#[clippy::has_significant_drop]
pub struct DisablePreemptGuard {
    local: Option<DisabledLocalIrqGuard>,
    marker: PhantomData<*mut ()>,
}

impl DisablePreemptGuard {
    pub fn new() -> Self {
        let pi = PREEMPT_INFO.num.get();
        PREEMPT_INFO.num.set(pi + 1);
        let local = (pi == 0).then(disable_local);
        atomic::compiler_fence(Acquire);
        Self {
            local,
            marker: PhantomData,
        }
    }
}

impl Default for DisablePreemptGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DisablePreemptGuard {
    fn drop(&mut self) {
        atomic::compiler_fence(Release);
        PREEMPT_INFO.num.set(PREEMPT_INFO.num.get() - 1);
    }
}

/// Whether the current CPU is in atomic context,
/// which means it holds some locks with irq disabled or is in irq context.
pub fn in_atomic() -> bool {
    PREEMPT_INFO.in_atomic()
}

/// Whether the current CPU is preemptible, which means it is neither in atomic context,
/// nor in IRQ context and the preemption is enabled.
/// If it is not preemptible, the CPU cannot call `schedule()`.
pub fn is_preemptible() -> bool {
    PREEMPT_INFO.is_preemptible()
}

pub fn is_in_preemption() -> bool {
    PREEMPT_INFO.in_preemption()
}

#[track_caller]
pub fn deactivate_preemption() {
    panic_if_not_preemptible(true);
    irq::disable_local();
    PREEMPT_INFO.in_preemption.set(Some(caller()));
    atomic::compiler_fence(Acquire);
}

pub fn activate_preemption() {
    assert!(!irq::is_local_enabled());
    atomic::compiler_fence(Release);
    PREEMPT_INFO.in_preemption.set(None);
    irq::enable_local();
    panic_if_not_preemptible(false);
}

// TODO: impl might_sleep
#[track_caller]
pub fn panic_if_in_atomic() {
    if !in_atomic() {
        return;
    }
    panic!(
        "CPU#{} is in atomic: atomic count = {}; suppressed location = {:?}.",
        this_cpu(),
        PREEMPT_INFO.num(),
        PREEMPT_INFO.in_preemption.get(),
    );
}

#[track_caller]
fn panic_if_not_preemptible(start: bool) {
    if is_preemptible() {
        return;
    }
    panic!(
        "CPU#{} is in not preemptible: atomic count = {}; suppressed location = {:?} ({}).",
        this_cpu(),
        PREEMPT_INFO.num(),
        PREEMPT_INFO.in_preemption.get(),
        if start { "start" } else { "end" }
    );
}
