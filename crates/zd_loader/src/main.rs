//! Static loader for the Zdroid rootfs engine.
//!
//! The kernel execs this binary for every guest process. The full
//! loader (ELF mapping, seccomp filter, SIGSYS trap handler, path
//! translation) lands in the engine chunks; this skeleton establishes
//! the libc-free, fixed-base build so the packaging pipeline can be
//! verified end to end.

#![no_std]
#![no_main]

use core::arch::{asm, global_asm};

const SYS_EXIT_GROUP: usize = 94;

global_asm!(
    ".globl _start",
    ".type _start, %function",
    "_start:",
    "mov x0, sp",
    "bl zd_main",
);

#[unsafe(no_mangle)]
extern "C" fn zd_main(_initial_stack: *const usize) -> ! {
    exit_group(0)
}

fn exit_group(code: usize) -> ! {
    unsafe {
        asm!(
            "svc #0",
            in("x8") SYS_EXIT_GROUP,
            in("x0") code,
            options(noreturn, nostack),
        );
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit_group(126)
}
