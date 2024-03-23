// SPDX-License-Identifier: MPL-2.0

use core::{cell::Cell, marker::PhantomData};

use crate::arch::irq::{disable_local as disable_local_irq, enable_local as enable_local_irq};

#[thread_local]
static PREEMPT_INFO: PreemptInfo = PreemptInfo::new();

// TODO: unify the fields to avoid the inconsistency.
#[derive(Debug)]
struct PreemptInfo {
    /// The number of locks and irq-disabled contexts held by the current CPU.
    num: Cell<usize>,
    in_preemption: Cell<bool>,
}

impl PreemptInfo {
    const fn new() -> Self {
        Self {
            num: Cell::new(0),
            in_preemption: Cell::new(false),
        }
    }

    fn num(&self) -> usize {
        self.num.get()
    }

    fn inc_num(&self) -> usize {
        let get = self.num.get();
        self.num.set(get + 1);
        get
    }

    fn dec_num(&self) {
        self.num.set(self.num.get() - 1);
    }

    fn in_preemption(&self) -> bool {
        self.in_preemption.get()
    }

    fn activate(&self) {
        self.in_preemption.set(false);
        enable_local_irq();
    }

    fn deactivate(&self) {
        if self.in_preemption.get() {
            panic!("Nested preemption is not allowed on in_preemption flag.");
        }
        disable_local_irq();
        self.in_preemption.set(true);
    }

    fn is_preemptible(&self) -> bool {
        // TODO: may cause inconsistency
        !self.in_preemption() && !self.in_atomic()
    }

    /// Whether the current CPU is in atomic context,
    /// which means it holds some locks or is in IRQ context.
    fn in_atomic(&self) -> bool {
        self.num() != 0
    }
}

/// A private type to prevent user from constructing DisablePreemptGuard directly.
struct _Guard(PhantomData<*mut ()>);

/// A guard to disable preempt.
#[clippy::has_significant_drop]
pub struct DisablePreemptGuard(PhantomData<*mut ()>);

impl DisablePreemptGuard {
    pub fn new() -> Self {
        PREEMPT_INFO.inc_num();
        Self(PhantomData)
    }

    /// Transfer this guard to a new guard.
    /// This guard must be dropped after this function.
    pub fn transfer_to(&self) -> Self {
        Self::new()
    }
}

impl Default for DisablePreemptGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DisablePreemptGuard {
    fn drop(&mut self) {
        PREEMPT_INFO.dec_num()
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

/// Allow preemption on the current CPU.
/// However, preemptible or not actually depends on the counter in `PREEMPT_INFO`.
pub fn activate_preemption() {
    PREEMPT_INFO.activate();
}

/// Disalbe all preemption on the current CPU.
pub fn deactivate_preemption() {
    PREEMPT_INFO.deactivate();
}

// TODO: impl might_sleep
#[track_caller]
pub fn panic_if_in_atomic() {
    if !in_atomic() {
        return;
    }
    panic!(
        "The CPU is in atomic: PREEMPT_INFO was {} with the in_preemption flag as {}.",
        PREEMPT_INFO.num(),
        PREEMPT_INFO.in_preemption()
    );
}

#[track_caller]
pub fn panic_if_not_preemptible() {
    if is_preemptible() {
        return;
    }
    panic!(
        "The CPU is not preemptible: PREEMPT_INFO was {} with the in_preemption flag as {}.",
        PREEMPT_INFO.num(),
        PREEMPT_INFO.in_preemption()
    );
}
