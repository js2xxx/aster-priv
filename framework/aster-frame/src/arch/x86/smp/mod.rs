//! Multiprocessor Boot Support
//!
//! The MP initialization protocol defines two classes of processors:
//! the bootstrap processor (BSP) and the application processors (APs).
//! Following a power-up or RESET of an MP system, system hardware dynamically
//! selects one of the processors on the system bus as the BSP. The remaining
//! processors are designated as APs.
//!
//! The BSP executes the BIOS's boot-strap code to configure the APIC environment,
//! sets up system-wide data structures. Up to now, BSP has completed most of the
//! initialization of the OS, but APs has not been awakened.
//!
//! Following a power-up or reset, the APs complete a minimal self-configuration,
//! then wait for a startup signal (a SIPI message) from the BSP processor.
//!
//! The wake-up of AP follows SNIT-SIPI-SIPI IPI sequence:
//! - Broadcast INIT IPI (Initialize the APs to the wait-for-SIPI state)
//! - Wait
//! - Broadcast De-assert INIT IPI (Only older processors need this step)
//! - Wait
//! - Broadcast SIPI IPI (APs exits the wait-for-SIPI state and starts executing code)
//! - Wait
//! - Broadcast SIPI IPI (If an AP fails to start)
//! This sequence does not need to be strictly followed, and there may be
//! different considerations in different systems.
use core::arch::global_asm;

use acpi::{
    platform::{Processor, ProcessorInfo},
    PlatformInfo,
};
use log::debug;

use super::kernel::apic::with_apic;
use crate::{
    arch::x86::{
        irq,
        kernel::{
            acpi::ACPI_TABLES,
            apic::{
                ApicId, DeliveryMode, DeliveryStatus, DestinationMode, DestinationShorthand, Icr,
                Level, TriggerMode,
            },
        },
        timer::read_monotonic_milli_seconds,
    },
    config::{KERNEL_OFFSET, PAGE_SIZE},
    vm::{paddr_to_vaddr, VmAllocOptions, VmSegment},
};

const AP_BOOT_START_PA: usize = 0x8000;
const AP_BOOT_STACK_SIZE: usize = PAGE_SIZE * 64;

global_asm!(include_str!("smp.S"));

/// Get processor information
///
/// This function needs to be called after the OS initializes the ACPI table.
pub(crate) fn get_processor_info() -> ProcessorInfo {
    PlatformInfo::new(&*ACPI_TABLES.get().unwrap().lock())
        .unwrap()
        .processor_info
        .unwrap()
}

/// Prepare the boot stack for the specified processor
///
/// This function needs to be called after initialization of the page allocator.
/// Currently, the address of the stack is placed at the location indexed by the processor ID
/// in the application processor boot frame.
pub(crate) fn prepare_boot_stacks(application_processor: &Processor) -> VmSegment {
    debug!("application processor info : {:?}", application_processor);
    let num_stack_frames = AP_BOOT_STACK_SIZE / PAGE_SIZE;
    let boot_stack_frames = VmAllocOptions::new(num_stack_frames)
        .is_contiguous(true)
        .uninit(false)
        .alloc_contiguous()
        .unwrap();
    let ap_stack_pointer = boot_stack_frames.end_paddr() + KERNEL_OFFSET;
    extern "C" {
        fn __ap_boot_stack_pointer_array();
    }
    debug!(
        "__ap_boot_stakc_top: 0x{:X}",
        __ap_boot_stack_pointer_array as usize
    );
    let ap_boot_stack_top: &mut usize = unsafe {
        &mut *(paddr_to_vaddr(
            __ap_boot_stack_pointer_array as usize
                + 8 * application_processor.local_apic_id as usize,
        ) as *mut usize)
    };
    *ap_boot_stack_top = ap_stack_pointer;
    debug!(
        "{} ap_boot_stack_top value: 0x{:X}",
        application_processor.local_apic_id, *ap_boot_stack_top
    );
    boot_stack_frames
}

/// Send IPIs to notify all application processors to boot
///
/// Follow the INIT-SIPI-SIPI IPI sequence.
/// Here, we don't check whether there is an AP that failed to start,
/// but send the second SIPI directly (checking whether each core is
/// started successfully one by one will bring extra overhead). For
/// APs that have been started, this signal will not bring any cost.
pub(crate) fn send_boot_ipis() {
    send_init_to_all_aps();
    wait_ms(10);
    send_init_deassert();
    wait_ms(2);
    send_startup_to_all_aps();
    wait_ms(2);
    // send_startup_to_all_aps();
    // wait_ms(2);
}

fn send_startup_to_all_aps() {
    let icr = Icr::new(
        ApicId::from(0),
        DestinationShorthand::AllExcludingSelf,
        TriggerMode::Egde,
        Level::Assert,
        DeliveryStatus::Idle,
        DestinationMode::Physical,
        DeliveryMode::StrartUp,
        (AP_BOOT_START_PA / PAGE_SIZE) as u8,
    );
    unsafe { with_apic(|apic| apic.send_ipi(icr)) };
}

fn send_init_to_all_aps() {
    let icr = Icr::new(
        ApicId::from(0),
        DestinationShorthand::AllExcludingSelf,
        TriggerMode::Level,
        Level::Assert,
        DeliveryStatus::Idle,
        DestinationMode::Physical,
        DeliveryMode::Init,
        0,
    );
    unsafe { with_apic(|apic| apic.send_ipi(icr)) };
}

fn send_init_deassert() {
    let icr = Icr::new(
        ApicId::from(0),
        DestinationShorthand::AllIncludingSelf,
        TriggerMode::Level,
        Level::Deassert,
        DeliveryStatus::Idle,
        DestinationMode::Physical,
        DeliveryMode::Init,
        0,
    );
    unsafe { with_apic(|apic| apic.send_ipi(icr)) };
}

fn wait_ms(ms: u64) {
    // Here we temporarily turn on the interrupt to ensure that
    // the timer works normally. However, after the timer ends,
    // the interrupt is still closed to avoid affecting the
    // initialization of other modules.
    irq::enable_local();
    let start_ms = read_monotonic_milli_seconds();
    while read_monotonic_milli_seconds() < start_ms + ms {
        core::hint::spin_loop();
    }
    irq::disable_local();
}
