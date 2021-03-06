// SPDX-License-Identifier: Apache-2.0

//! The SEV shim
//!
//! This crate contains the system/kernel that handles the syscalls (and cpuid instructions)
//! from the enclave code and might proxy them to the host.

#![no_std]
#![deny(clippy::all)]
#![deny(clippy::integer_arithmetic)]
#![deny(missing_docs)]
#![no_main]
#![feature(asm, naked_functions)]

extern crate compiler_builtins;
extern crate rcrt1;

#[macro_use]
pub mod print;

pub mod addr;
pub mod allocator;
pub mod asm;
pub mod attestation;
pub mod gdt;
pub mod hostcall;
/// Shared components for the shim and the loader
pub mod hostlib;
pub mod hostmap;
pub mod no_std;
pub mod pagetables;
pub mod paging;
pub mod payload;
pub mod random;
pub mod shim_stack;
pub mod spin;
mod start;
pub mod syscall;
pub mod usermode;

use crate::addr::{ShimPhysUnencryptedAddr, ShimVirtAddr, SHIM_VIRT_OFFSET};
use crate::attestation::SEV_SECRET;
use crate::hostlib::BootInfo;
use crate::pagetables::switch_sallyport_to_unencrypted;
use crate::paging::SHIM_PAGETABLE;
use crate::payload::PAYLOAD_VIRT_ADDR;
use crate::print::{enable_printing, is_printing_enabled};
use core::convert::TryFrom;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use primordial::Address;
use sallyport::Block;
use spinning::RwLock;
use x86_64::structures::paging::Translate;
use x86_64::VirtAddr;

static C_BIT_MASK: AtomicU64 = AtomicU64::new(0);

static BOOT_INFO: RwLock<Option<BootInfo>> =
    RwLock::<Option<BootInfo>>::const_new(spinning::RawRwLock::const_new(), None);

static SHIM_HOSTCALL_PHYS_ADDR: RwLock<Option<usize>> =
    RwLock::<Option<usize>>::const_new(spinning::RawRwLock::const_new(), None);

static mut PAYLOAD_READY: AtomicBool = AtomicBool::new(false);

/// Get the SEV C-Bit mask
#[inline(always)]
pub fn get_cbit_mask() -> u64 {
    C_BIT_MASK.load(Ordering::Relaxed)
}

/// Switch the stack and jump to a function
///
/// # Safety
///
/// This function is unsafe, because the caller has to ensure a 16 byte
/// aligned usable stack.
#[allow(clippy::integer_arithmetic)]
pub unsafe fn switch_shim_stack(ip: extern "C" fn() -> !, sp: u64) -> ! {
    assert_eq!(sp % 16, 0);
    asm!("
        mov rsp, {SP}
        sub rsp, 8
        push rbp
        call {IP}
        ",
        SP = in(reg) sp,
        IP = in(reg) ip,
        options(noreturn, nomem)
    );
}

/// Defines the entry point function.
///
/// The function must have the signature `extern "C" fn() -> !`.
///
/// This macro just creates a function named `_start_main`, which the assembler
/// stub will use as the entry point. The advantage of using this macro instead
/// of providing an own `_start_main` function is that the macro ensures that the
/// function and argument types are correct and that the global variables, which
/// are needed later on, are initialized.
macro_rules! entry_point {
    ($path:path) => {
        #[doc(hidden)]
        #[export_name = "_start_main"]
        pub unsafe extern "C" fn __impl_start(boot_info: *mut BootInfo, c_bit_mask: u64) -> ! {
            // validate the signature of the program entry point
            let f: extern "C" fn() -> ! = $path;

            C_BIT_MASK.store(c_bit_mask, Ordering::Relaxed);

            SHIM_HOSTCALL_PHYS_ADDR.write().replace({
                let address = Address::<u64, Block>::from(boot_info as *mut Block);
                let shim_virt = ShimVirtAddr::<Block>::try_from(address).unwrap();
                ShimPhysUnencryptedAddr::<Block>::try_from(shim_virt)
                    .unwrap()
                    .into_mut() as *mut _ as _
            });

            // make a local copy of boot_info, before the shared page gets overwritten
            BOOT_INFO.write().replace(boot_info.read());

            SEV_SECRET.write().copy_from_bootinfo(boot_info);

            switch_sallyport_to_unencrypted(c_bit_mask);

            // Everything setup, so print works
            enable_printing();

            // Switch the stack to a guarded stack
            switch_shim_stack(f, gdt::INITIAL_STACK.pointer.as_u64())
        }
    };
}

entry_point!(shim_main);

/// The entry point for the shim
pub extern "C" fn shim_main() -> ! {
    unsafe { gdt::init() };
    payload::execute_payload()
}

/// The panic function
///
/// Called, whenever somethings panics.
///
/// Reverts to a triple fault, which causes a `#VMEXIT` and a KVM shutdown,
/// if it can't print the panic and exit normally with an error code.
#[panic_handler]
pub fn panic(info: &core::panic::PanicInfo) -> ! {
    use asm::_enarx_asm_triple_fault;

    static mut ALREADY_IN_PANIC: AtomicBool = AtomicBool::new(false);

    // Don't print anything, if the FRAME_ALLOCATOR is not yet initialized
    unsafe {
        if is_printing_enabled()
            && ALREADY_IN_PANIC
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            print::_eprint(format_args!("{}\n", info));
            stack_trace();
            // FIXME: might want to have a custom panic hostcall
            hostcall::shim_exit(255);
        }
    }

    // provoke triple fault, causing a VM shutdown
    unsafe { _enarx_asm_triple_fault() };
}

#[inline(never)]
unsafe fn stack_trace() {
    let mut rbp: usize;

    asm!("mov {}, rbp", out(reg) rbp);

    print::_eprint(format_args!("TRACE:\n"));

    if SHIM_PAGETABLE.try_read().is_none() {
        SHIM_PAGETABLE.force_unlock_write()
    }

    if BOOT_INFO.try_read().is_none() {
        BOOT_INFO.force_unlock_write();
    }

    let bootinfo = BOOT_INFO.read();
    let shim_start = bootinfo.unwrap().shim.start;
    let shim_offset = shim_start.checked_add(SHIM_VIRT_OFFSET as _).unwrap();

    let active_table = SHIM_PAGETABLE.read();

    //Maximum 64 frames
    for _frame in 0..64 {
        if let Some(rip_rbp) = rbp.checked_add(size_of::<usize>() as _) {
            if active_table
                .translate_addr(VirtAddr::new(rbp as _))
                .is_some()
                && active_table
                    .translate_addr(VirtAddr::new(rip_rbp as _))
                    .is_some()
            {
                let rip = *(rip_rbp as *const usize);
                if let Some(rip) = rip.checked_sub(1) {
                    if rip == 0 {
                        break;
                    }

                    if let Some(rip) = rip.checked_sub(shim_offset) {
                        print::_eprint(format_args!("  0x{:>016x}\n", rip));
                        rbp = *(rbp as *const usize);
                    } else if PAYLOAD_READY.load(Ordering::Relaxed) {
                        if let Some(rip) = rip.checked_sub(PAYLOAD_VIRT_ADDR.read().as_u64() as _) {
                            print::_eprint(format_args!("P 0x{:>016x}\n", rip));
                            rbp = *(rbp as *const usize);
                        } else {
                            break;
                        }
                    }
                } else {
                    // RIP zero
                    break;
                }
            } else {
                // RBP NOT MAPPED
                break;
            }
        } else {
            // RBP OVERFLOW
            break;
        }
    }
}
