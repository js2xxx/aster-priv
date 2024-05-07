use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Once;

use crate::{
    arch::{
        self, enable_common_cpu_features,
        smp::{get_processor_info, prepare_boot_stacks, send_boot_ipis},
    },
    cpu, trap,
    vm::VmSegment,
};

static AP_BOOT_INFO: Once<BTreeMap<u32, ApBootInfo>> = Once::new();

struct ApBootInfo {
    is_started: AtomicBool,
    // TODO: When the AP starts up and begins executing tasks, the boot stack will
    // no longer be used, and the `VmSegment` can be deallocated (this problem also
    // exists in the boot processor, but the memory it occupies should be returned
    // to the frame allocator).
    boot_stack_frames: VmSegment,
}

pub(crate) static CPUNUM: Once<u32> = Once::new();
pub(crate) static BSP: Once<u32> = Once::new();

/// Only initialize the processor number here to facilitate the system
/// to pre-allocate some data structures according to this number.
pub(crate) fn init() {
    let processor_info = get_processor_info();
    let num_aps = processor_info.application_processors.len();
    CPUNUM.call_once(|| (num_aps + 1) as u32);
    BSP.call_once(crate::cpu::this_cpu);

    boot_all_aps();
}

/// Boot all application processors.
///
/// This function should be called late in the system startup.
/// The system must at least ensure that the scheduler, ACPI table, memory allocation,
/// and IPI module have been initialized.
fn boot_all_aps() {
    AP_BOOT_INFO.call_once(|| {
        let processor_info = get_processor_info();
        let iter = processor_info.application_processors.iter();
        iter.map(|ap| {
            let ap_boot_info = ApBootInfo {
                is_started: AtomicBool::new(false),
                boot_stack_frames: prepare_boot_stacks(ap),
            };
            (ap.local_apic_id, ap_boot_info)
        })
        .collect()
    });
    send_boot_ipis();
}

#[no_mangle]
fn ap_early_entry(local_apic_id: u32) -> ! {
    enable_common_cpu_features();
    cpu::ap_init(local_apic_id);
    trap::init();
    arch::init_ap();

    let ap_boot_info = AP_BOOT_INFO.get().unwrap();
    ap_boot_info
        .get(&local_apic_id)
        .unwrap()
        .is_started
        .store(true, Ordering::Release);

    log::info!("CPU#{} is online", crate::cpu::this_cpu());

    extern "C" {
        fn __aster_ap_entry() -> !;
    }
    unsafe { __aster_ap_entry() }
}
