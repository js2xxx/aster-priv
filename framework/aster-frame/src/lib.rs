// SPDX-License-Identifier: MPL-2.0

//! The framework part of Asterinas.
#![allow(internal_features)]
#![feature(alloc_error_handler)]
#![feature(allow_internal_unstable)]
#![feature(const_mut_refs)]
#![feature(const_ptr_sub_ptr)]
#![feature(const_trait_impl)]
#![feature(coroutines)]
#![feature(iter_from_coroutine)]
#![feature(let_chains)]
#![feature(negative_impls)]
#![feature(new_uninit)]
#![feature(offset_of)]
#![feature(panic_info_message)]
#![feature(ptr_sub_ptr)]
#![feature(strict_provenance)]
#![feature(thread_local)]
#![allow(dead_code)]
#![allow(unused_variables)]
#![no_std]

extern crate alloc;
#[cfg(ktest)]
#[macro_use]
extern crate ktest;
#[macro_use]
extern crate static_assertions;

pub mod arch;
pub mod boot;
pub mod bus;
pub mod config;
pub mod console;
pub mod cpu;
mod error;
pub mod io_mem;
pub mod logger;
#[cfg(target_os = "none")]
pub mod panicking;
pub mod prelude;
pub mod smp;
pub mod sync;
pub mod task;
pub mod timer;
pub mod trap;
pub mod user;
mod util;
pub mod vm;

#[cfg(feature = "intel_tdx")]
use tdx_guest::init_tdx;

pub use self::{cpu::CpuLocal, error::Error, prelude::Result};

pub fn init() {
    arch::before_all_init();
    // Safety: Cpu local data has not been accessed before
    unsafe { cpu::bsp_init() };
    logger::init();
    #[cfg(feature = "intel_tdx")]
    let td_info = init_tdx().unwrap();
    #[cfg(feature = "intel_tdx")]
    early_println!(
        "td gpaw: {}, td attributes: {:?}\nTDX guest is initialized",
        td_info.gpaw,
        td_info.attributes
    );
    vm::heap_allocator::init();
    boot::init();
    vm::init();
    trap::init();
    task::init();
    arch::mid_init();
    smp::init();
    arch::after_all_init();
    bus::init();
    invoke_ffi_init_funcs();
}

// Aborts the QEMU
pub fn abort() -> ! {
    crate::arch::qemu::exit_qemu(crate::arch::qemu::QemuExitCode::Failed);
}

fn invoke_ffi_init_funcs() {
    extern "C" {
        fn __sinit_array();
        fn __einit_array();
    }
    let call_len = (__einit_array as usize - __sinit_array as usize) / 8;
    for i in 0..call_len {
        unsafe {
            let function = (__sinit_array as usize + 8 * i) as *const fn();
            (*function)();
        }
    }
}

/// Simple unit tests for the ktest framework.
#[cfg(ktest)]
mod test {
    #[ktest]
    fn trivial_assertion() {
        assert_eq!(0, 0);
    }

    #[ktest]
    #[should_panic]
    fn failing_assertion() {
        assert_eq!(0, 1);
    }

    #[ktest]
    #[should_panic(expected = "expected panic message")]
    fn expect_panic() {
        panic!("expected panic message");
    }
}
