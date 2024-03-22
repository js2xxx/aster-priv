// SPDX-License-Identifier: MPL-2.0

//! CPU.

use alloc::vec::Vec;
use core::{
    cell::{Cell, RefCell, UnsafeCell},
    ptr,
};

use bitvec::{order::Lsb0, slice::IterOnes, vec::BitVec};
use x86::msr::{wrmsr, IA32_TSC_AUX};

use crate::{config::PAGE_SIZE, trap::disable_local, vm::VmAllocOptions};

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")]{
        pub use trapframe::GeneralRegs;
        pub use crate::arch::x86::cpu::*;
    }
}

/// Returns the number of CPUs.
pub fn num_cpus() -> u32 {
    crate::smp::CPUNUM.get().copied().unwrap_or(1)
}

/// Returns the ID of this CPU.
pub fn this_cpu() -> u32 {
    unsafe { x86::time::rdtscp().1 }
}

#[derive(Default)]
pub struct CpuSet {
    bitset: BitVec,
}

impl CpuSet {
    pub fn new_full() -> Self {
        let num_cpus = num_cpus();
        let mut bitset = BitVec::with_capacity(num_cpus as usize);
        bitset.resize(num_cpus as usize, true);
        Self { bitset }
    }

    pub fn new_empty() -> Self {
        let num_cpus = num_cpus();
        let mut bitset = BitVec::with_capacity(num_cpus as usize);
        bitset.resize(num_cpus as usize, false);
        Self { bitset }
    }

    pub fn single(cpu_id: u32) -> Self {
        let mut set = Self::new_empty();
        set.add(cpu_id);
        set
    }

    pub fn add(&mut self, cpu_id: u32) {
        self.bitset.set(cpu_id as usize, true);
    }

    pub fn add_from_vec(&mut self, cpu_ids: Vec<u32>) {
        for cpu_id in cpu_ids {
            self.add(cpu_id)
        }
    }

    pub fn add_all(&mut self) {
        self.bitset.fill(true);
    }

    pub fn remove(&mut self, cpu_id: u32) {
        self.bitset.set(cpu_id as usize, false);
    }

    pub fn remove_from_vec(&mut self, cpu_ids: Vec<u32>) {
        for cpu_id in cpu_ids {
            self.remove(cpu_id);
        }
    }

    pub fn clear(&mut self) {
        self.bitset.fill(false);
    }

    pub fn contains(&self, cpu_id: u32) -> bool {
        self.bitset.get(cpu_id as usize).as_deref() == Some(&true)
    }

    pub fn iter(&self) -> IterOnes<'_, usize, Lsb0> {
        self.bitset.iter_ones()
    }
}

/// Defines a CPU-local variable.
///
/// # Example
///
/// ```rust
/// use crate::cpu_local;
/// use core::cell::RefCell;
///
/// cpu_local! {
///     static FOO: RefCell<u32> = RefCell::new(1);
///
///     #[allow(unused)]
///     pub static BAR: RefCell<f32> = RefCell::new(1.0);
/// }
/// CpuLocal::borrow_with(&FOO, |val| {
///     println!("FOO VAL: {:?}", *val);
/// })
///
/// ```
#[macro_export]
#[allow_internal_unstable(thread_local)]
macro_rules! cpu_local {
    // empty
    () => {};

    // multiple declarations
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr; $($rest:tt)*) => {
        $(#[$attr])* #[thread_local]
        $vis static $name: $crate::CpuLocal<$t> = unsafe { $crate::CpuLocal::new($init) };
        $crate::cpu_local!($($rest)*);
    };

    // single declaration
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr) => (
        // TODO: reimplement cpu-local variable to support multi-core
        $(#[$attr])* #[thread_local]
        $vis static $name: $crate::CpuLocal<$t> = $crate::CpuLocal::new($init);
    );
}

/// CPU-local objects.
///
/// A CPU-local object only gives you immutable references to the underlying value.
/// To mutate the value, one can use atomic values (e.g., `AtomicU32`) or internally mutable
/// objects (e.g., `RefCell`).
///
/// The `CpuLocal<T: Sync>` can be used directly.
/// Otherwise, the `CpuLocal<T>` must be used through `CpuLocal::borrow_with`.
///
/// TODO: re-implement `CpuLocal`
pub struct CpuLocal<T>(UnsafeCell<T>);

impl<T> CpuLocal<T> {
    /// Initialize CPU-local object
    /// Developer cannot construct a valid CpuLocal object arbitrarily
    #[allow(clippy::missing_safety_doc)]
    pub const unsafe fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    /// Borrow an immutable reference to the underlying value and feed it to a closure.
    ///
    /// During the execution of the closure, local IRQs are disabled. This ensures that
    /// the CPU-local object is only accessed by the current task or IRQ handler.
    /// As local IRQs are disabled, one should keep the closure as short as possible.
    pub fn with<U, F: FnOnce(&T) -> U>(&self, f: F) -> U {
        // Disable interrupts when accessing cpu-local variable
        // Preemption is also disabled in `disable_local()`.
        let _guard = disable_local();
        // Safety. Now that the local IRQs are disabled, this CPU-local object can only be
        // accessed by the current task/thread. So it is safe to get its immutable reference
        // regardless of whether `T` implements `Sync` or not.
        let val_ref = unsafe { self.do_borrow() };
        f(val_ref)
    }

    unsafe fn do_borrow(&self) -> &T {
        &*self.0.get()
    }
}

impl<T> CpuLocal<Cell<T>> {
    pub fn get(&self) -> T
    where
        T: Copy,
    {
        self.with(|c| c.get())
    }

    pub fn set(&self, value: T) {
        self.with(|c| c.set(value))
    }

    pub fn replace(&self, value: T) -> T {
        self.with(|c| c.replace(value))
    }

    pub fn take(&self) -> T
    where
        T: Default,
    {
        self.with(|c| c.take())
    }
}

impl<T> CpuLocal<RefCell<T>> {
    pub fn with_borrow<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        self.with(|c| f(&c.borrow()))
    }

    pub fn with_borrow_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        self.with(|c| f(&mut c.borrow_mut()))
    }
}

extern "C" {
    fn __stdata();
    fn __tdata_size();
    fn __tbss_size();
}

unsafe fn cpu_local_init(addr: *mut u8) -> *mut u8 {
    let tdata = addr;
    let tdata_size = __tdata_size as usize;
    let tbss = addr.add(tdata_size);
    let tbss_size = __tbss_size as usize;
    let tend = tbss.add(tbss_size);

    ptr::copy_nonoverlapping(__stdata as *const u8, tdata, tdata_size);
    ptr::write_bytes(tbss, 0, tbss_size);
    tend.cast::<usize>().write(tend as usize);
    tend
}

/// Initializes the local CPU data for the bootstrap processor (BSP).
/// During the initialization of the local data in BSP, the frame allocator has
/// not been initialized yet, thus a reserved memory segment is used as its local storage.
///
/// # Safety
///
/// It must be guaranteed that the BSP will not access local data before this function is called,
/// otherwise dereference of non-Pod is undefined behavior.
pub unsafe fn bsp_init() {
    extern "C" {
        fn __stbsp();
    }
    let base = __stbsp as *mut u8;
    let tptr = unsafe { cpu_local_init(base) };
    // Safety: FS register is not used for any other purpose.
    unsafe { set_cpu_local_base_addr(tptr as usize) };

    wrmsr(IA32_TSC_AUX, 0);
}

pub fn ap_init(cpu_id: u32) {
    let cpu_local_size = __tdata_size as usize + __tbss_size as usize;

    let local_segment = VmAllocOptions::new((cpu_local_size + PAGE_SIZE - 1) / PAGE_SIZE)
        .is_contiguous(true)
        .need_dealloc(false)
        .alloc_contiguous()
        .unwrap();
    let tptr = unsafe { cpu_local_init(local_segment.as_mut_ptr()) };
    // Safety: `local_segment` is initialized to have the `need_dealloc` attribute, so this
    // memory will be dedicated to storing cpu local data.
    unsafe { set_cpu_local_base_addr(tptr as usize) };

    unsafe { wrmsr(IA32_TSC_AUX, cpu_id.into()) };
    core::mem::forget(local_segment);
}
