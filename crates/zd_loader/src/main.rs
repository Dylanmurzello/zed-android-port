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
    /// Guest program path.
    program: &'static [u8],
    /// String the guest sees as its `argv[0]`.
    argv0: *const u8,
    /// `argv` index where the guest's own arguments start.
    first_arg: usize,
    /// Explicit form: loader entries precede the guest's in argv, so the
    /// kernel's argument area is compacted to hide them.
    compact: bool,
    selftest: bool,
}

/// Two ways in: `ZD_PROGRAM` set means the kernel exec'd the loader with
/// the guest's own argv (the supervisor's form, exact `cmdline`); else
/// the loader's basename must be one of its own names and the program
/// follows its flags (`zdload [--selftest] [-0 argv0] <program> ...`).
fn parse_invocation(stack: &InitialStack) -> Invocation {
    let config = config::get();
    if !config.program.is_empty() {
        return Invocation {
            program: config.program.as_bytes(),
            argv0: stack.argv.first().copied().unwrap_or(config.program.as_ptr()),
            first_arg: 1,
            compact: false,
            selftest: config.selftest,
        };
    }
    let argv0 = stack.arg(0).unwrap_or(b"");
    let name = path::basename(argv0);
    if !LOADER_SELF_NAMES.iter().any(|self_name| self_name.as_bytes() == name) {
        abort!(
            "invoked as {}: symlink-farm dispatch needs the guest PATH search, which is not in this build",
            Bytes(name)
        );
    }
    let mut index = 1usize;
    let mut selftest = config.selftest;
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
    let (Some(program), Some(&program_pointer)) = (stack.arg(index), stack.argv.get(index)) else {
        abort!("usage: {} [--selftest] [-0 argv0] <guest program> [args...]", Bytes(name));
    };
    Invocation {
        program,
        argv0: argv0_override.unwrap_or(program_pointer),
        first_arg: index + 1,
        compact: true,
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
    let program = invocation.program;

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

    set_process_name(program);
    let new_sp = build_guest_vector(&stack, &invocation, &loaded);
    // SAFETY: the guest image and its stack are in place.
    unsafe { jump(loaded.start_address(), new_sp) }
}

/// Write the guest's `[argc][argv][0][envp][0][auxv]` into the gap below
/// the kernel's vector and return the guest's initial stack pointer.
///
/// The argument strings are compacted in place as well: the kernel
/// reports `[arg_start, arg_end)` as `/proc/<pid>/cmdline`, and that
/// area still spells the loader's own argv. Moving the guest's strings
/// to its front (every move goes to a lower address, in order, so no
/// source is clobbered before it is copied) makes the process look like
/// the guest to `ps` and to programs that re-read their command line;
/// the unused tail is NUL-filled. Environment strings stay where they
/// are.
fn build_guest_vector(stack: &InitialStack, invocation: &Invocation, loaded: &load::Loaded) -> usize {
    let config = config::get();
    let argc = 1 + stack.argv.len().saturating_sub(invocation.first_arg);
    let mut envc = 0usize;
    for entry in stack.env_entries() {
        if !config::hidden_from_guest(entry) {
            envc += 1;
        }
    }
    envc += config.guest_env.entries().count();
    let auxc = stack.auxv.len();
    let words = VectorWriter::words_needed(argc, envc, auxc);
    let bytes = words * 8;
    if bytes + 64 > VECTOR_GAP {
        abort!("argument vector too large ({words} words)");
    }
    let new_sp = ((stack.top as usize) - bytes) & !15;
    // SAFETY: `[new_sp, top)` lies in the gap `_start` reserved below the
    // kernel's vector; nothing else uses it.
    let out = unsafe { core::slice::from_raw_parts_mut(new_sp as *mut u64, words) };

    let compact = invocation.compact && !stack.argv.is_empty() && {
        let arg_start = stack.argv[0] as usize;
        let arg_end = argument_area_end(stack);
        // SAFETY: kernel-provided argument strings.
        let guest_len: usize = unsafe {
            cstr(invocation.argv0).len()
                + 1
                + stack
                    .argv
                    .iter()
                    .skip(invocation.first_arg)
                    .map(|&argument| cstr(argument).len() + 1)
                    .sum::<usize>()
        };
        guest_len <= arg_end - arg_start
    };

    let mut write = || -> Result<(), zd_engine_core::stack::StackError> {
        let mut cursor = if compact { stack.argv[0] as usize } else { 0 };
        let mut place = |pointer: *const u8| -> u64 {
            if !compact {
                return pointer as u64;
            }
            // SAFETY: `pointer` is one of the kernel's argument strings
            // inside the area, and the destination never passes it.
            let len = unsafe { cstr(pointer) }.len() + 1;
            unsafe { core::ptr::copy(pointer, cursor as *mut u8, len) };
            let placed = cursor;
            cursor += len;
            placed as u64
        };

        let mut writer = VectorWriter::new(out, argc)?;
        writer.push_arg(place(invocation.argv0))?;
        for &argument in stack.argv.iter().skip(invocation.first_arg) {
            writer.push_arg(place(argument))?;
        }
        writer.end_args()?;
        for &pointer in stack.envp {
            // SAFETY: kernel-provided environment strings.
            let entry = unsafe { cstr(pointer) };
            if !config::hidden_from_guest(entry) {
                writer.push_env(pointer as u64)?;
            }
        }
        for entry in config.guest_env.entries() {
            writer.push_env(entry.as_ptr() as u64)?;
        }
        writer.end_env()?;
        if compact {
            let arg_end = argument_area_end(stack);
            // SAFETY: the tail of the argument area.
            unsafe { core::ptr::write_bytes(cursor as *mut u8, 0, arg_end - cursor) };
        }
        // The recorded guest path lives in loader `.bss`, which outlives
        // the argument strings that compaction may have moved.
        let execfn = config.guest_exe.as_ptr() as u64;
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
    let auxv_offset = 8 * (1 + argc + 1 + envc + 1);
    config::set_auxv(new_sp + auxv_offset, (auxc + 1) * 16);
    new_sp
}

/// End of the kernel's argument-string area: where the environment
/// strings begin, or past the last argument when there are none.
fn argument_area_end(stack: &InitialStack) -> usize {
    match stack.envp.first() {
        Some(&first_env) => first_env as usize,
        None => {
            let last = stack.argv[stack.argv.len() - 1];
            // SAFETY: kernel-provided argument string.
            last as usize + unsafe { cstr(last) }.len() + 1
        }
    }
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
