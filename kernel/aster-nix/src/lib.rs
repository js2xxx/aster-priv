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
#![feature(offset_of)]
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

    println!(
        "[kernel] Spawn init thread, tid = {}",
        current_thread!().tid()
    );
    // net::lazy_init();
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
    // thread::work_queue::init();

    print_banner();

    let karg = boot::kernel_cmdline();

    let initproc = process::Process::spawn_user_process(
        karg.get_initproc_path().unwrap(),
        karg.get_initproc_argv().to_vec(),
        karg.get_initproc_envp().to_vec(),
    )
    .expect("Run init process failed.");

    // Wait till initproc become zombie.
    initproc.main_thread().unwrap().join();

    // TODO: exit via qemu isa debug device should not be the only way.
    let exit_code = if initproc.exit_code().unwrap() == 0 {
        QemuExitCode::Success
    } else {
        QemuExitCode::Failed
    };
    exit_qemu(exit_code);

    // let t = Thread::spawn_kernel_thread(
    //     ThreadOptions::new(crate::tests::test_priorities),
    // );
    // t.join();

    // exit_qemu(QemuExitCode::Success)
}

static START_SPAWNING: AtomicBool = AtomicBool::new(false);

/// first process never return
#[controlled]
pub fn run_first_process() -> ! {
    START_SPAWNING.store(true, atomic::Ordering::Release);
    Thread::spawn_kernel_thread(
        ThreadOptions::new(init_thread).cpu_affinity(CpuSet::single(this_cpu())),
    );
    loop {
        aster_frame::task::schedule();
    }
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
    loop {
        aster_frame::task::schedule();
    }
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

mod tests {
    use alloc::{sync::Arc, vec::Vec};
    use core::{
        array,
        sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
    };

    use aster_frame::{
        arch::{current_tick, read_tsc, TIMER_FREQ},
        cpu::{this_cpu, CpuSet},
        sync::{Mutex, WaitQueue},
        task::Priority,
        trap::is_local_enabled,
    };

    use crate::{
        print, println,
        thread::{
            kernel_thread::{KernelThreadExt, ThreadOptions},
            Thread,
        },
    };

    struct Barrier {
        flag: AtomicBool,
        wq: WaitQueue,
    }

    impl Barrier {
        fn new() -> Self {
            Barrier {
                flag: AtomicBool::new(false),
                wq: WaitQueue::new(),
            }
        }

        fn test(&self) -> bool {
            self.flag.load(SeqCst)
        }

        fn wait(&self) {
            self.wq.wait_until(|| self.test().then_some(()))
        }

        fn set(&self) {
            self.flag.store(true, SeqCst);
            self.wq.wake_all();
        }
    }

    pub fn test_priorities() {
        const WAIT_SECS: u64 = 80;
        const PRIO: &[u16] = &[115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125];
        const COUNT: usize = PRIO.len();

        assert!(is_local_enabled());

        struct Data {
            mutexes: [Mutex<()>; COUNT],
            start: Barrier,
            stop: Barrier,
            counter: [AtomicU64; COUNT],
        }

        println!("Creating data");
        let data = Arc::new(Data {
            mutexes: array::from_fn(|_| Mutex::new(())),
            start: Barrier::new(),
            stop: Barrier::new(),
            counter: array::from_fn(|_| AtomicU64::new(0)),
        });

        let spawn = |id: usize, prio, data: Arc<Data>| {
            let func = move || {
                println!("Spawned #{id}");
                data.start.wait();
                while !data.stop.test() {
                    let idx = read_tsc() as usize % COUNT;
                    let _lock = data.mutexes[idx].lock();
                    data.counter[id].fetch_add(1, SeqCst);
                }
            };

            Thread::spawn_kernel_thread(
                ThreadOptions::new(func)
                    .cpu_affinity(CpuSet::single(this_cpu()))
                    .priority(prio),
            )
        };

        println!("Spawning threads");
        let threads = PRIO
            .iter()
            .enumerate()
            .map(|(id, &prio)| spawn(id, Priority::new(prio), data.clone()))
            .collect::<Vec<_>>();

        println!("Waiting for threads");
        data.start.set();
        let start = current_tick();
        loop {
            let ticks = current_tick() - start;
            if ticks >= WAIT_SECS * TIMER_FREQ {
                break;
            }
            Thread::yield_now();
        }

        println!("Stopping threads");
        data.stop.set();
        threads.into_iter().for_each(|t| t.join());

        data.counter
            .iter()
            .zip(PRIO)
            .for_each(|(count, &prio)| print!("{}, ", count.load(SeqCst)));
        println!();
    }
}
