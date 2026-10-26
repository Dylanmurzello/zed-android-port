//! `memcpy` and friends. The loader links without a libc, and
//! `compiler_builtins` does not export the memory symbols on Android
//! targets, so the compiler's own calls to them land here. Written in
//! assembly so no optimisation can turn a byte loop back into a call to
//! the function being defined.

use core::arch::global_asm;

global_asm!(
    ".globl memcpy",
    ".type memcpy, %function",
    "memcpy:",
    "mov x3, x0",
    "cbz x2, 2f",
    "1:",
    "ldrb w4, [x1], #1",
    "strb w4, [x3], #1",
    "subs x2, x2, #1",
    "b.ne 1b",
    "2:",
    "ret",
    ".size memcpy, . - memcpy",
    //
    ".globl memmove",
    ".type memmove, %function",
    "memmove:",
    "cmp x0, x1",
    "b.ls memcpy",
    "add x3, x1, x2",
    "cmp x0, x3",
    "b.hs memcpy",
    "mov x3, x0",
    "cbz x2, 2f",
    "add x4, x1, x2",
    "add x5, x0, x2",
    "1:",
    "ldrb w6, [x4, #-1]!",
    "strb w6, [x5, #-1]!",
    "subs x2, x2, #1",
    "b.ne 1b",
    "2:",
    "ret",
    ".size memmove, . - memmove",
    //
    ".globl memset",
    ".type memset, %function",
    "memset:",
    "mov x3, x0",
    "cbz x2, 2f",
    "1:",
    "strb w1, [x3], #1",
    "subs x2, x2, #1",
    "b.ne 1b",
    "2:",
    "ret",
    ".size memset, . - memset",
    //
    ".globl memcmp",
    ".type memcmp, %function",
    ".globl bcmp",
    ".type bcmp, %function",
    "memcmp:",
    "bcmp:",
    "cbz x2, 3f",
    "1:",
    "ldrb w3, [x0], #1",
    "ldrb w4, [x1], #1",
    "cmp w3, w4",
    "b.ne 2f",
    "subs x2, x2, #1",
    "b.ne 1b",
    "3:",
    "mov w0, #0",
    "ret",
    "2:",
    "sub w0, w3, w4",
    "ret",
    ".size memcmp, . - memcmp",
    //
    ".globl strlen",
    ".type strlen, %function",
    "strlen:",
    "mov x1, x0",
    "1:",
    "ldrb w2, [x1], #1",
    "cbnz w2, 1b",
    "sub x0, x1, x0",
    "sub x0, x0, #1",
    "ret",
    ".size strlen, . - strlen",
);

/// `core` is prebuilt with unwinding and keeps a reference to the
/// personality routine in its landing pads even though this binary
/// aborts on panic and never unwinds through them.
#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
