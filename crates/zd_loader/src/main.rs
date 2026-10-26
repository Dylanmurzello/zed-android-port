//! Static loader for the Zdroid rootfs engine.
//!
//! The kernel execs this binary for every guest process. It installs the
//! SIGSYS handler and the seccomp filter, maps the guest executable and
//! its dynamic linker from the rootfs, rebuilds the initial stack for the
//! guest, and jumps. It stays mapped at its fixed base afterwards: its
//! text is where every translated syscall is re-issued from.

#![no_std]
#![no_main]

#[macro_use]
mod log;
mod config;
mod handler;
mod load;
mod mem;
mod selftest;
mod sys;

use core::arch::{asm, global_asm};

use zd_engine_core::stack::{
    AT_BASE, AT_ENTRY, AT_EXECFN, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM, AuxEntry,
    VectorWriter, auxv_get,
};
use zd_engine_core::{LOADER_SELF_NAMES, elf::PHDR_SIZE, path};

use crate::log::Bytes;

/// Bytes `_start` leaves between the kernel's vector and the loader's
/// first frame; the guest's vector is rebuilt in that gap.
pub const VECTOR_GAP: usize = 0x40000;

global_asm!(
    ".globl _start",
    ".type _start, %function",
    "_start:",
    "mov x0, sp",
    "mov x1, x0",
    "sub x1, x1, #0x40000",
    "mov sp, x1",
    "bl zd_main",
    "brk #0",
    ".size _start, . - _start",
    //
    // Leaf syscall thunk: the self-test maps a second copy of the text
    // and calls this function there to issue an `svc` from outside the
    // seccomp window.
    ".globl zd_raw_svc",
    ".type zd_raw_svc, %function",
    "zd_raw_svc:",
    "mov x8, x6",
    "svc #0",
    "ret",
    ".size zd_raw_svc, . - zd_raw_svc",
);

unsafe extern "C" {
    pub fn zd_raw_svc(
        a0: usize,
        a1: usize,
        a2: usize,
        a3: usize,
        a4: usize,
        a5: usize,
        number: usize,
    ) -> isize;
}

/// The vector the kernel built: `argc`, `argv`, `envp`, `auxv`.
pub struct InitialStack {
    pub top: *const u64,
    pub argv: &'static [*const u8],
    pub envp: &'static [*const u8],
    pub auxv: &'static [AuxEntry],
}

impl InitialStack {
    /// # Safety
    /// `sp` must be the stack pointer the kernel handed to `_start`.
    unsafe fn parse(sp: *const u64) -> Self {
        unsafe {
            let argc = *sp as usize;
            let argv = sp.add(1) as *const *const u8;
            let envp = argv.add(argc + 1);
            let mut envc = 0usize;
            while !(*envp.add(envc)).is_null() {
                envc += 1;
            }
            let auxv = envp.add(envc + 1) as *const AuxEntry;
            let mut auxc = 0usize;
            while (*auxv.add(auxc))[0] != AT_NULL {
                auxc += 1;
            }
            Self {
                top: sp,
                argv: core::slice::from_raw_parts(argv, argc),
                envp: core::slice::from_raw_parts(envp, envc),
                auxv: core::slice::from_raw_parts(auxv, auxc),
            }
        }
    }

    fn arg(&self, index: usize) -> Option<&'static [u8]> {
        self.argv.get(index).map(|&pointer| unsafe { cstr(pointer) })
    }

    fn env_entries(&self) -> impl Iterator<Item = &'static [u8]> + '_ {
        self.envp.iter().map(|&pointer| unsafe { cstr(pointer) })
    }
}

/// # Safety
/// `pointer` must address a NUL-terminated string that outlives `'a`.
pub unsafe fn cstr<'a>(pointer: *const u8) -> &'a [u8] {
    unsafe {
        let mut len = 0usize;
        while *pointer.add(len) != 0 {
            len += 1;
        }
        core::slice::from_raw_parts(pointer, len)
    }
}

struct Invocation {
    /// `argv` index of the guest program path.
    program: usize,
    /// String the guest sees as its `argv[0]`.
    argv0: *const u8,
    /// `argv` index where the guest's own arguments start.
    first_arg: usize,
    selftest: bool,
}

fn parse_invocation(stack: &InitialStack) -> Invocation {
    let argv0 = stack.arg(0).unwrap_or(b"");
    let name = path::basename(argv0);
    if !LOADER_SELF_NAMES.iter().any(|self_name| self_name.as_bytes() == name) {
        abort!(
            "invoked as {}: symlink-farm dispatch needs the guest PATH search, which is not in this build",
            Bytes(name)
        );
    }
    let mut index = 1usize;
    let mut selftest = false;
    let mut argv0_override: Option<*const u8> = None;
    loop {
        match stack.arg(index) {
            Some(b"--selftest") => {
                selftest = true;
                index += 1;
            }
            Some(b"-0") => {
                argv0_override = stack.argv.get(index + 1).copied();
                index += 2;
            }
            _ => break,
        }
    }
    let Some(&program) = stack.argv.get(index) else {
        abort!("usage: {} [--selftest] [-0 argv0] <guest program> [args...]", Bytes(name));
    };
    Invocation {
        program: index,
        argv0: argv0_override.unwrap_or(program),
        first_arg: index + 1,
        selftest,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn zd_main(sp: *const u64) -> ! {
    // SAFETY: `_start` passes the kernel's initial stack pointer.
    let stack = unsafe { InitialStack::parse(sp) };

    // Nothing before this point may trap; the handler is the first thing
    // installed so a filter inherited from a guest parent is already
    // served when the probe runs.
    if let Err(errno) = handler::install() {
        abort!("sigaction(SIGSYS) failed: errno {errno}");
    }
    if let Err(errno) = handler::unblock_sigsys() {
        abort!("sigprocmask(SIG_UNBLOCK, SIGSYS) failed: errno {errno}");
    }

    let page_size = auxv_get(stack.auxv, AT_PAGESZ).unwrap_or(4096) as usize;
    if let Err(error) = config::init(stack.env_entries(), page_size) {
        abort!("{error}");
    }
    let invocation = parse_invocation(&stack);
    let Some(program) = stack.arg(invocation.program) else {
        abort!("missing guest program");
    };

    let filter = match handler::ensure_filter() {
        Ok(state) => state,
        Err(error) => abort!(
            "seccomp filter: {} failed: errno {}",
            error.stage,
            error.errno
        ),
    };

    if invocation.selftest {
        selftest::preamble(&stack, &filter, program);
    }
    let started = selftest::now_ns();
    let loaded = match load::load(program) {
        Ok(loaded) => loaded,
        Err(error) => abort!("cannot load {}: {error}", Bytes(program)),
    };
    config::set_guest_exe(program);
    if invocation.selftest {
        selftest::report_load(program, &loaded, started);
    }

    let new_sp = build_guest_vector(&stack, &invocation, &loaded);
    set_process_name(program);
    // SAFETY: the guest image and its stack are in place.
    unsafe { jump(loaded.start_address(), new_sp) }
}

/// Write the guest's `[argc][argv][0][envp][0][auxv]` into the gap below
/// the kernel's vector and return the guest's initial stack pointer. The
/// strings stay where the kernel put them; only pointers move.
fn build_guest_vector(stack: &InitialStack, invocation: &Invocation, loaded: &load::Loaded) -> usize {
    let config = config::get();
    let argc = 1 + stack.argv.len().saturating_sub(invocation.first_arg);
    let mut envc = 0usize;
    for entry in stack.env_entries() {
        if !config.guest_env.overrides(entry) {
            envc += 1;
        }
    }
    envc += config.guest_env.entries().count();
    let words = VectorWriter::words_needed(argc, envc, stack.auxv.len());
    let bytes = words * 8;
    if bytes + 64 > VECTOR_GAP {
        abort!("argument vector too large ({words} words)");
    }
    let new_sp = ((stack.top as usize) - bytes) & !15;
    // SAFETY: `[new_sp, top)` lies in the gap `_start` reserved below the
    // kernel's vector; nothing else uses it.
    let out = unsafe { core::slice::from_raw_parts_mut(new_sp as *mut u64, words) };

    let execfn = stack.argv[invocation.program] as u64;
    let mut write = || -> Result<(), zd_engine_core::stack::StackError> {
        let mut writer = VectorWriter::new(out, argc)?;
        writer.push_arg(invocation.argv0 as u64)?;
        for &argument in stack.argv.iter().skip(invocation.first_arg) {
            writer.push_arg(argument as u64)?;
        }
        writer.end_args()?;
        for &pointer in stack.envp {
            // SAFETY: kernel-provided environment strings.
            let entry = unsafe { cstr(pointer) };
            if !config.guest_env.overrides(entry) {
                writer.push_env(pointer as u64)?;
            }
        }
        for entry in config.guest_env.entries() {
            writer.push_env(entry.as_ptr() as u64)?;
        }
        writer.end_env()?;
        for &[key, value] in stack.auxv {
            let value = match key {
                AT_PHDR => loaded.phdr,
                AT_PHENT => PHDR_SIZE as u64,
                AT_PHNUM => loaded.phnum,
                AT_ENTRY => loaded.entry,
                AT_BASE => loaded.interp_base,
                AT_EXECFN => execfn,
                _ => value,
            };
            writer.push_aux([key, value])?;
        }
        writer.finish()?;
        Ok(())
    };
    if write().is_err() {
        abort!("argument vector does not fit its reservation");
    }
    new_sp
}

fn set_process_name(program: &[u8]) {
    let name = path::basename(program);
    let mut buffer = [0u8; 16];
    let len = name.len().min(15);
    buffer[..len].copy_from_slice(&name[..len]);
    if let Err(_errno) = sys::prctl(sys::PR_SET_NAME, buffer.as_ptr() as usize, 0, 0, 0) {
        // Cosmetic: `ps` shows the loader's name instead.
    }
}

/// # Safety
/// `entry` must be the mapped guest's (or its interpreter's) entry point
/// and `new_sp` a valid, 16-byte aligned initial stack.
unsafe fn jump(entry: u64, new_sp: usize) -> ! {
    unsafe {
        asm!(
            "mov sp, x17",
            "mov x0, xzr",
            "mov x1, xzr",
            "mov x2, xzr",
            "mov x3, xzr",
            "mov x4, xzr",
            "mov x5, xzr",
            "mov x6, xzr",
            "mov x7, xzr",
            "mov x8, xzr",
            "mov x9, xzr",
            "mov x10, xzr",
            "mov x11, xzr",
            "mov x12, xzr",
            "mov x13, xzr",
            "mov x14, xzr",
            "mov x15, xzr",
            "mov x17, xzr",
            "mov x18, xzr",
            "mov x19, xzr",
            "mov x20, xzr",
            "mov x21, xzr",
            "mov x22, xzr",
            "mov x23, xzr",
            "mov x24, xzr",
            "mov x25, xzr",
            "mov x26, xzr",
            "mov x27, xzr",
            "mov x28, xzr",
            "mov x29, xzr",
            "mov x30, xzr",
            "br x16",
            in("x16") entry,
            in("x17") new_sp,
            options(noreturn),
        );
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    abort!("panic: {info}")
}
