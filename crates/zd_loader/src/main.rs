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
mod exec;
mod handler;
mod load;
mod mem;
mod selftest;
mod sys;

use core::arch::{asm, global_asm};

use zd_engine_core::shebang::{self, MAX_DEPTH};
use zd_engine_core::stack::{
    AT_BASE, AT_EGID, AT_ENTRY, AT_EUID, AT_EXECFN, AT_GID, AT_NULL, AT_PAGESZ, AT_PHDR,
    AT_PHENT, AT_PHNUM, AT_UID, AuxEntry, VectorWriter, auxv_get,
};
use zd_engine_core::translate::Translated;
use zd_engine_core::{LOADER_SELF_NAMES, elf::PHDR_SIZE, path};

use crate::log::Bytes;

/// Bytes `_start` leaves between the kernel's vector and the loader's
/// first frame; the guest's vector is rebuilt in that gap.
pub const VECTOR_GAP: usize = 0x40000;

/// Shell to run when the loader is invoked under one of its own names
/// with no program (the integrated terminal's `$SHELL`).
const DEFAULT_SHELL: &[u8] = b"/bin/bash";

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

/// Room for `[interpreter, argument]` per `#!` level plus the program.
const MAX_LEADING: usize = 2 * MAX_DEPTH + 2;

struct Invocation {
    /// Strings that open the guest's argv: its `argv[0]`, or after a
    /// `#!` chain, interpreters, their arguments and the script path.
    leading: [*const u8; MAX_LEADING],
    leading_len: usize,
    /// `argv` index where the guest's own arguments start.
    first_arg: usize,
    /// Explicit form: loader entries precede the guest's in argv, so the
    /// kernel's argument area is compacted to hide them.
    compact: bool,
    selftest: bool,
}

/// Three ways in. `ZD_PROGRAM` set: the kernel exec'd the loader with the
/// guest's own argv (the supervisor's form, exact `cmdline`). The
/// loader's own basename: `zdload [--selftest] [-0 argv0] <program>
/// args...`, or with no program (or a leading `-` flag) the default shell
/// with those flags, as `$SHELL` is invoked. Any other basename: a
/// symlink-farm entry, resolved on the guest `PATH`.
fn resolve_invocation(stack: &InitialStack) -> Invocation {
    let config = config::get();
    let mut invocation = Invocation {
        leading: [core::ptr::null(); MAX_LEADING],
        leading_len: 1,
        first_arg: 1,
        compact: false,
        selftest: config.selftest,
    };
    if !config.program.is_empty() {
        set_level0(config.program.as_bytes());
        invocation.leading[0] = stack
            .argv
            .first()
            .copied()
            .unwrap_or(config::get().levels[0].path.as_ptr());
        return invocation;
    }
    let argv0 = stack.arg(0).unwrap_or(b"");
    let name = path::basename(argv0);
    if LOADER_SELF_NAMES.iter().any(|self_name| self_name.as_bytes() == name) {
        let mut index = 1usize;
        let mut argv0_override: Option<*const u8> = None;
        loop {
            match stack.arg(index) {
                Some(b"--selftest") => {
                    invocation.selftest = true;
                    index += 1;
                }
                Some(b"-0") => {
                    argv0_override = stack.argv.get(index + 1).copied();
                    index += 2;
                }
                _ => break,
            }
        }
        match (stack.arg(index), stack.argv.get(index)) {
            (Some(program), Some(&program_pointer)) if !program.starts_with(b"-") => {
                set_level0(program);
                invocation.leading[0] = argv0_override.unwrap_or(program_pointer);
                invocation.first_arg = index + 1;
                invocation.compact = true;
            }
            _ => {
                set_level0(DEFAULT_SHELL);
                invocation.leading[0] = argv0_override.unwrap_or(config::get().levels[0].path.as_ptr());
                invocation.first_arg = index;
            }
        }
        return invocation;
    }
    if !search_guest_path(name) {
        log::abort_code(127, format_args!("{}: command not found on the guest PATH", Bytes(name)));
    }
    invocation.leading[0] = stack.argv[0];
    invocation
}

fn set_level0(program: &[u8]) {
    // SAFETY: single-threaded startup.
    let config = unsafe { config::get_mut() };
    if config.levels[0].path.set(program).is_err() {
        abort!("program path too long");
    }
}

/// Resolve a bare name on the guest `PATH` into `levels[0].path`.
fn search_guest_path(name: &[u8]) -> bool {
    let Some(translator) = config::translator() else {
        return false;
    };
    let guest_path = config::get().guest_path.as_bytes();
    let mut candidate = [0u8; path::MAX_PATH];
    let mut host = [0u8; path::MAX_PATH];
    for directory in guest_path.split(|&byte| byte == b':') {
        let directory: &[u8] = if directory.is_empty() { b"." } else { directory };
        let mut writer = path::Writer::new(&mut candidate);
        if writer.push(directory).is_err() || writer.push_component(name).is_err() {
            continue;
        }
        let len = writer.finish();
        let pointer = match translator.guest_to_host(&candidate[..len], &mut host) {
            Ok(Translated::Host(_)) => host.as_ptr(),
            Ok(Translated::Relative) => candidate.as_ptr(),
            _ => continue,
        };
        let Ok(mode) = sys::stat_mode(sys::AT_FDCWD, pointer, 0) else {
            continue;
        };
        if mode & sys::S_IFMT != sys::S_IFREG || sys::faccessat(sys::AT_FDCWD, pointer, sys::X_OK).is_err() {
            continue;
        }
        set_level0(&candidate[..len]);
        return true;
    }
    false
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
    let mut invocation = resolve_invocation(&stack);

    let filter = match handler::ensure_filter() {
        Ok(state) => state,
        Err(error) => abort!(
            "seccomp filter: {} failed: errno {}",
            error.stage,
            error.errno
        ),
    };

    let program = config::get().levels[0].path.as_bytes();
    if invocation.selftest {
        selftest::preamble(&stack, &filter, program);
    }
    let started = selftest::now_ns();
    let loaded = load_chain(&mut invocation);
    if invocation.selftest {
        selftest::report_load(program, &loaded, started);
    }

    set_process_name(program);
    let new_sp = build_guest_vector(&stack, &invocation, &loaded);
    // SAFETY: the guest image and its stack are in place.
    unsafe { jump(loaded.start_address(), new_sp) }
}

/// Open `levels[0].path`, follow `#!` lines (each one prepends the
/// interpreter and its argument to the guest's argv and makes the file
/// just opened the next argument, as the kernel does), and map the ELF
/// at the end of the chain.
fn load_chain(invocation: &mut Invocation) -> load::Loaded {
    let Some(translator) = config::translator() else {
        abort!("no translator");
    };
    let mut depth = 0usize;
    loop {
        let program = config::get().levels[depth].path.as_bytes();
        let mut host = [0u8; path::MAX_PATH];
        let fd = match load::open_guest(translator, program, &mut host) {
            Ok(fd) => fd,
            Err(load::LoadError::Open(errno)) => log::abort_code(
                if errno == sys::ENOENT { 127 } else { 126 },
                format_args!("cannot open {}: errno {errno}", Bytes(program)),
            ),
            Err(error) => abort!("cannot open {}: {error}", Bytes(program)),
        };
        let mut head = [0u8; shebang::LINE_LIMIT];
        let head_len = match load::read_head(fd, &mut head) {
            Ok(len) => len,
            Err(error) => abort!("cannot read {}: {error}", Bytes(program)),
        };
        match shebang::parse(&head[..head_len]) {
            Some(script) => {
                load::close_quietly(fd);
                if depth == MAX_DEPTH {
                    log::abort_code(126, format_args!("{}: too many levels of #!", Bytes(program)));
                }
                push_level(invocation, depth, &script);
                depth += 1;
            }
            None => {
                let loaded = load::load_fd(fd);
                load::close_quietly(fd);
                let loaded = match loaded {
                    Ok(loaded) => loaded,
                    Err(error) => abort!("cannot load {}: {error}", Bytes(program)),
                };
                config::set_guest_exe(program);
                return loaded;
            }
        }
    }
}

/// `#!interp arg` at `levels[depth]`: argv becomes
/// `[interp, arg?, levels[depth].path, <previous argv minus its first>]`.
fn push_level(invocation: &mut Invocation, depth: usize, script: &shebang::Shebang<'_>) {
    // SAFETY: single-threaded startup.
    let config = unsafe { config::get_mut() };
    let level = &mut config.levels[depth + 1];
    if level.path.set(script.interpreter).is_err()
        || level.argument.set(script.argument.unwrap_or(b"")).is_err()
    {
        abort!("#! line too long");
    }
    let added = if script.argument.is_some() { 2 } else { 1 };
    if invocation.leading_len + added > MAX_LEADING {
        abort!("#! chain too deep");
    }
    let leading = &mut invocation.leading;
    for index in (1..invocation.leading_len).rev() {
        leading[index + added] = leading[index];
    }
    leading[0] = level.path.as_ptr();
    if script.argument.is_some() {
        leading[1] = level.argument.as_ptr();
    }
    leading[added] = config.levels[depth].path.as_ptr();
    invocation.leading_len += added;
    // A `#!` chain mixes strings from loader .bss into argv; those never
    // came from the kernel's argument area, so it cannot be compacted.
    invocation.compact = false;
}

/// Write the guest's `[argc][argv][0][envp][0][auxv]` into the gap below
/// the kernel's vector and return the guest's initial stack pointer.
///
/// In the explicit form the argument strings are compacted in place as
/// well: the kernel reports `[arg_start, arg_end)` as
/// `/proc/<pid>/cmdline`, and that area spells the loader's own argv.
/// Moving the guest's strings to its front (every move goes to a lower
/// address, in order, so no source is clobbered before it is copied)
/// makes the process look like the guest to `ps`; the unused tail is
/// NUL-filled. Environment strings stay where they are.
fn build_guest_vector(stack: &InitialStack, invocation: &Invocation, loaded: &load::Loaded) -> usize {
    let config = config::get();
    let argc = invocation.leading_len + stack.argv.len().saturating_sub(invocation.first_arg);
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

    let compact = invocation.compact && invocation.leading_len == 1 && !stack.argv.is_empty() && {
        let arg_start = stack.argv[0] as usize;
        let arg_end = argument_area_end(stack);
        let first = invocation.leading[0] as usize;
        // SAFETY: kernel-provided argument strings.
        let guest_len: usize = unsafe {
            cstr(invocation.leading[0]).len()
                + 1
                + stack
                    .argv
                    .iter()
                    .skip(invocation.first_arg)
                    .map(|&argument| cstr(argument).len() + 1)
                    .sum::<usize>()
        };
        first >= arg_start && first < arg_end && guest_len <= arg_end - arg_start
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
        for &pointer in &invocation.leading[..invocation.leading_len] {
            writer.push_arg(place(pointer))?;
        }
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
        // AT_EXECFN is the path asked for (the script of a `#!` chain);
        // it lives in loader .bss, which outlives moved argument strings.
        let execfn = config.levels[0].path.as_ptr() as u64;
        for &[key, value] in stack.auxv {
            let value = match key {
                AT_PHDR => loaded.phdr,
                AT_PHENT => PHDR_SIZE as u64,
                AT_PHNUM => loaded.phnum,
                AT_ENTRY => loaded.entry,
                AT_BASE => loaded.interp_base,
                AT_EXECFN => execfn,
                AT_UID | AT_EUID | AT_GID | AT_EGID if config.fake_root => 0,
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
