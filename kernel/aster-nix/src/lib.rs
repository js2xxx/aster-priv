// SPDX-License-Identifier: MPL-2.0

//! The std library of Asterinas.
#![no_std]
#![deny(unsafe_code)]
#![allow(dead_code)]
#![allow(unused_variables)]
#![feature(btree_cursors)]
#![feature(btree_extract_if)]
#![feature(const_option)]
#![feature(exclusive_range_pattern)]
#![feature(extend_one)]
#![feature(format_args_nl)]
#![feature(int_roundings)]
#![feature(let_chains)]
#![feature(linked_list_remove)]
#![feature(register_tool)]
#![feature(step_trait)]
#![feature(trait_alias)]
#![register_tool(component_access_control)]

use core::{hint, sync::atomic::AtomicBool};

use aster_frame::{
    arch::qemu::{exit_qemu, QemuExitCode},
    boot,
    cpu::{this_cpu, CpuSet},
    task::Priority,
};
use process::Process;

use crate::{
    prelude::*,
    thread::{
        kernel_thread::{KernelThreadExt, ThreadOptions},
        Thread,
    },
};

extern crate alloc;
extern crate lru;
#[macro_use]
extern crate controlled;
#[cfg(ktest)]
#[macro_use]
extern crate ktest;
#[macro_use]
extern crate getset;

pub mod console;
pub mod device;
pub mod driver;
pub mod error;
pub mod events;
pub mod fs;
pub mod net;
pub mod prelude;
mod process;
mod sched;
pub mod syscall;
pub mod thread;
pub mod time;
mod util;
pub(crate) mod vdso;
pub mod vm;

pub fn init() {
    driver::init();
    net::init();
    sched::init();
    fs::rootfs::init(boot::initramfs()).unwrap();
    device::init().unwrap();
    vdso::init();
}

fn init_thread() {
    println!(
        "[kernel] Spawn init thread, tid = {}",
        current_thread!().tid()
    );
    net::lazy_init();
    fs::lazy_init();
    // driver::pci::virtio::block::block_device_test();
    let thread = Thread::spawn_kernel_thread(ThreadOptions::new(|| {
        println!("[kernel] Hello world from kernel!");
        let current = current_thread!();
        let tid = current.tid();
        debug!("current tid = {}", tid);
    }));
    thread.join();
    info!(
        "[aster-nix/lib.rs] spawn kernel thread, tid = {}",
        thread.tid()
    );
    thread::work_queue::init();

    print_banner();

    let karg = boot::kernel_cmdline();

    let initproc = Process::spawn_user_process(
        karg.get_initproc_path().unwrap(),
        karg.get_initproc_argv().to_vec(),
        karg.get_initproc_envp().to_vec(),
    )
    .expect("Run init process failed.");

    // Wait till initproc become zombie.
    idle_while(|| !initproc.is_zombie());

    // TODO: exit via qemu isa debug device should not be the only way.
    let exit_code = if initproc.exit_code().unwrap() == 0 {
        QemuExitCode::Success
    } else {
        QemuExitCode::Failed
    };
    exit_qemu(exit_code);
}

static START_SPAWNING: AtomicBool = AtomicBool::new(false);

/// first process never return
#[controlled]
pub fn run_first_process() -> ! {
    START_SPAWNING.store(true, atomic::Ordering::Release);
    Thread::spawn_kernel_thread(
        ThreadOptions::new(init_thread)
            .cpu_affinity(CpuSet::single(this_cpu()))
            .priority(Priority::lowest()),
    );
    unreachable!()
}

#[no_mangle]
#[allow(unsafe_code)]
fn __aster_ap_entry() -> ! {
    while !START_SPAWNING.load(atomic::Ordering::Acquire) {
        hint::spin_loop()
    }
    let thread_main = || {
        println!(
            "[kernel] Spawn idle thread for CPU#{}, tid = {}",
            this_cpu(),
            current_thread!().tid()
        );
        idle_while(|| true);
        unreachable!()
    };
    Thread::spawn_kernel_thread(
        ThreadOptions::new(thread_main)
            .cpu_affinity(CpuSet::single(this_cpu()))
            .priority(Priority::lowest()),
    );
    unreachable!()
}

fn print_banner() {
    println!("\x1B[36m");
    println!(
        r"
   _   ___ _____ ___ ___ ___ _  _   _   ___
  /_\ / __|_   _| __| _ \_ _| \| | /_\ / __|
 / _ \\__ \ | | | _||   /| || .` |/ _ \\__ \
/_/ \_\___/ |_| |___|_|_\___|_|\_/_/ \_\___/
"
    );
    println!("\x1B[0m");
}

fn idle_while(mut cond: impl FnMut() -> bool) {
    const MAX_SPIN_SHIFT: usize = 16;
    const MAX_YIELD_COUNT: usize = 61;

    let mut spin_shift = 0;
    let mut yield_count = 0;
    let cpu = this_cpu();
    loop {
        if !cond() {
            break;
        }
        if Thread::yield_now() {
            spin_shift = 0;
            if yield_count == MAX_YIELD_COUNT {
                yield_count = 0;
            } else {
                yield_count += 1;
                continue;
            }
        }

        for _ in 0..(1 << spin_shift) {
            hint::spin_loop();
        }
        if spin_shift < MAX_SPIN_SHIFT {
            spin_shift += 1;
        }
        if !cond() {
            break;
        }
        aster_frame::task::trigger_load_balancing();
    }
}
