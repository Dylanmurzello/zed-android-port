//! `--selftest`: print the facts the chunk-1 verification needs, as
//! `zd-selftest: <record> key=value ...` lines on stderr, then benchmark
//! the trap path against the native path.
//!
//! The trapped benchmark needs an `svc` outside the loader's address
//! window. Anonymous executable memory is avoided: the loader maps its
//! own text segment a second time from the APK file at a kernel-chosen
//! address and calls the copy of `zd_raw_svc` found there.

use core::arch::asm;

use zd_engine_core::elf::{MAX_PHDRS, PF_X, PHDR_SIZE, PT_LOAD, ProgramHeader, page_ceil, page_floor};
use zd_engine_core::path::MAX_PATH;
use zd_engine_core::stack::{AT_PHDR, AT_PHNUM, auxv_get};
use zd_engine_core::sysno::nr;
use zd_engine_core::translate::Translated;
use zd_engine_core::{LOADER_IMAGE_BASE, LOADER_WINDOW_SIZE};

use crate::handler::{COMPAT_COUNT, FilterState, TRAP_COUNT};
use crate::load::Loaded;
use crate::log::Bytes;
use crate::{InitialStack, config, sys, zd_raw_svc};

type Stub = unsafe extern "C" fn(usize, usize, usize, usize, usize, usize, usize) -> isize;

/// Stub kept for the post-load checks; written once in `preamble`.
static STUB: config::Global<Option<Stub>> = config::Global::new(None);

const SAMPLES: usize = 2048;
const PROBE_STACK: usize = 64 * 1024;
const FILL: u8 = 0xA5;
/// What one trap may take from an alternate stack, kernel frame
/// included: musl registers 12 KiB and Rust 16 KiB, and a guest handler
/// running there needs room of its own.
const FRAME_BUDGET: usize = 10 * 1024;

/// Largest frame the probe saw; read back by `report_load`.
static FRAME_MAX: config::Global<usize> = config::Global::new(0);
const OPS_PER_SAMPLE: u64 = 8;

fn counter() -> u64 {
    let value: u64;
    // SAFETY: reading the virtual counter is permitted from EL0 on Linux.
    unsafe {
        asm!("isb", "mrs {0}, cntvct_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

fn counter_frequency() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {0}, cntfrq_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

pub fn now_ns() -> u64 {
    ticks_to_ns(counter())
}

fn ticks_to_ns(ticks: u64) -> u64 {
    let frequency = counter_frequency().max(1);
    ((ticks as u128) * 1_000_000_000u128 / frequency as u128) as u64
}

fn loader_text(stack: &InitialStack) -> Option<ProgramHeader> {
    let phdr = auxv_get(stack.auxv, AT_PHDR)?;
    let phnum = auxv_get(stack.auxv, AT_PHNUM)? as usize;
    if phnum > MAX_PHDRS {
        return None;
    }
    // SAFETY: AT_PHDR describes this loader's own program headers, which
    // the kernel mapped with the image.
    let table = unsafe { core::slice::from_raw_parts(phdr as *const u8, phnum * PHDR_SIZE) };
    let mut phdrs = [ProgramHeader::default(); MAX_PHDRS];
    let phdrs = ProgramHeader::parse_table(table, phnum, &mut phdrs).ok()?;
    phdrs
        .iter()
        .find(|phdr| phdr.p_type == PT_LOAD && phdr.p_flags & PF_X != 0)
        .copied()
}

fn outside_window_stub(stack: &InitialStack) -> Result<Stub, &'static str> {
    let text = loader_text(stack).ok_or("loader text segment not found via AT_PHDR")?;
    let page = config::get().page_size as u64;
    let loader_path = config::get().loader_path.as_ptr();
    if config::get().loader_path.is_empty() {
        return Err("loader path unknown");
    }
    let fd = sys::openat(sys::AT_FDCWD, loader_path, sys::O_RDONLY | sys::O_CLOEXEC, 0)
        .map_err(|_| "cannot reopen the loader file")?;
    let file_start = page_floor(text.p_offset, page);
    let len = page_ceil(text.p_offset + text.p_filesz, page) - file_start;
    let copy = sys::mmap(
        0,
        len as usize,
        sys::PROT_READ | sys::PROT_EXEC,
        sys::MAP_PRIVATE,
        fd,
        file_start,
    );
    if let Err(_errno) = sys::close(fd) {
        // The mapping keeps the file alive.
    }
    let copy = copy.map_err(|_| "cannot map the loader text a second time")?;
    let function = (zd_raw_svc as Stub) as usize as u64;
    let stub = copy as u64 + (function - page_floor(text.p_vaddr, page));
    if (LOADER_IMAGE_BASE..LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE).contains(&stub) {
        return Err("stub copy landed inside the window");
    }
    // SAFETY: the copy holds the same bytes as `zd_raw_svc`, a leaf
    // function with no relocations.
    Ok(unsafe { core::mem::transmute::<usize, Stub>(stub as usize) })
}

struct Bench {
    median_ns: u64,
    p99_ns: u64,
    min_ns: u64,
    traps: u64,
}

fn bench(samples: &mut [u64; SAMPLES], mut operation: impl FnMut()) -> Bench {
    let traps_before = TRAP_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    for sample in samples.iter_mut() {
        let start = counter();
        for _ in 0..OPS_PER_SAMPLE {
            operation();
        }
        *sample = (counter() - start) / OPS_PER_SAMPLE;
    }
    let traps = TRAP_COUNT.load(core::sync::atomic::Ordering::Relaxed) - traps_before;
    samples.sort_unstable();
    Bench {
        median_ns: ticks_to_ns(samples[SAMPLES / 2]),
        p99_ns: ticks_to_ns(samples[SAMPLES * 99 / 100]),
        min_ns: ticks_to_ns(samples[0]),
        traps,
    }
}

fn report(name: &str, result: Bench) {
    emit_line!(
        "zd-selftest: bench name={name} n={SAMPLES} ops={OPS_PER_SAMPLE} median_ns={} p99_ns={} min_ns={} traps={}",
        result.median_ns,
        result.p99_ns,
        result.min_ns,
        result.traps
    );
}

pub fn preamble(stack: &InitialStack, filter: &FilterState, program: &[u8]) {
    let config = config::get();
    let top = stack.top as u64;
    let va_bits = 64 - top.leading_zeros();
    let (text_start, text_end) = loader_text(stack)
        .map(|text| (text.p_vaddr, text.p_vaddr + text.p_memsz))
        .unwrap_or((0, 0));
    emit_line!(
        "zd-selftest: env page={} va_bits={va_bits} text={text_start:#x}-{text_end:#x} window={:#x}-{:#x} pid={} rootfs={} loader={}",
        config.page_size,
        LOADER_IMAGE_BASE,
        LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE,
        sys::getpid(),
        Bytes(config.rootfs.as_bytes()),
        Bytes(config.loader_path.as_bytes()),
    );
    let (state, tsync) = match filter {
        FilterState::Inherited => ("inherited", "n/a"),
        FilterState::Installed { tsync: true } => ("installed", "ok"),
        FilterState::Installed { tsync: false } => ("installed", "fallback"),
    };
    emit_line!(
        "zd-selftest: filter state={state} probe=ack tsync={tsync} compat_traps={}",
        COMPAT_COUNT.load(core::sync::atomic::Ordering::Relaxed)
    );

    let mut samples = [0u64; SAMPLES];
    report(
        "probe_trap",
        bench(&mut samples, || {
            if !crate::handler::probe() {
                crate::abort!("probe stopped acking mid-benchmark");
            }
        }),
    );

    let Some(translator) = config::translator() else {
        return;
    };
    let mut host = [0u8; MAX_PATH];
    let mut guest = [0u8; MAX_PATH];
    guest[..program.len()].copy_from_slice(program);
    let host_ok = matches!(
        translator.guest_to_host(program, &mut host),
        Ok(Translated::Host(_))
    );
    if host_ok {
        report(
            "openat_native",
            bench(&mut samples, || {
                match sys::openat(sys::AT_FDCWD, host.as_ptr(), sys::O_RDONLY | sys::O_CLOEXEC, 0) {
                    Ok(fd) => {
                        if let Err(_errno) = sys::close(fd) {}
                    }
                    Err(errno) => crate::abort!("openat_native failed: errno {errno}"),
                }
            }),
        );
    }
    match outside_window_stub(stack) {
        Ok(stub) => {
            // SAFETY: single-threaded startup; read back by `report_load`.
            unsafe { *STUB.get() = Some(stub) };
            report(
                "openat_trap",
                bench(&mut samples, || {
                    // SAFETY: the stub is `zd_raw_svc` at another address.
                    let ret = unsafe {
                        stub(
                            sys::AT_FDCWD as usize,
                            guest.as_ptr() as usize,
                            sys::O_RDONLY | sys::O_CLOEXEC,
                            0,
                            0,
                            0,
                            nr::OPENAT as usize,
                        )
                    };
                    match sys::check(ret) {
                        Ok(fd) => {
                            if let Err(_errno) = sys::close(fd as i32) {}
                        }
                        Err(errno) => crate::abort!("openat_trap failed: errno {errno}"),
                    }
                }),
            );
            let mut stat_buffer = [0u8; 256];
            report(
                "fstatat_trap",
                bench(&mut samples, || {
                    let ret = unsafe {
                        stub(
                            sys::AT_FDCWD as usize,
                            guest.as_ptr() as usize,
                            stat_buffer.as_mut_ptr() as usize,
                            0,
                            0,
                            0,
                            nr::NEWFSTATAT as usize,
                        )
                    };
                    if let Err(errno) = sys::check(ret) {
                        crate::abort!("fstatat_trap failed: errno {errno}");
                    }
                }),
            );
        }
        Err(reason) => emit_line!("zd-selftest: bench name=openat_trap skipped={reason}"),
    }
    // SAFETY: written just above when the stub was built.
    if let Some(stub) = unsafe { *STUB.get() } {
        frame_probe(stub, &guest);
    }
}

/// Bytes of an alternate stack one trap consumes, the kernel's frame
/// plus the handler's: the lowest byte a delivery touched on a stack
/// pre-filled with a pattern. The handler is installed with SA_ONSTACK,
/// so the smallest alternate stack a guest registers bounds what the
/// handler may use. The probe stack is unregistered again before the
/// jump into the guest.
fn frame_probe(stub: Stub, guest: &[u8; MAX_PATH]) {
    let stack = match sys::mmap(
        0,
        PROBE_STACK,
        sys::PROT_READ | sys::PROT_WRITE,
        sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
        -1,
        0,
    ) {
        Ok(address) => address,
        Err(errno) => {
            emit_line!("zd-selftest: frame skipped=mmap errno {errno}");
            return;
        }
    };
    // SAFETY: a fresh private mapping of PROBE_STACK bytes.
    let region = unsafe { core::slice::from_raw_parts_mut(stack as *mut u8, PROBE_STACK) };
    let mut largest = 0usize;
    largest = largest.max(measure(region, "probe", || {
        if !crate::handler::probe() {
            crate::abort!("probe stopped acking in the frame probe");
        }
    }));
    largest = largest.max(measure(region, "openat", || {
        let ret = unsafe {
            stub(
                sys::AT_FDCWD as usize,
                guest.as_ptr() as usize,
                sys::O_RDONLY | sys::O_CLOEXEC,
                0,
                0,
                0,
                nr::OPENAT as usize,
            )
        };
        match sys::check(ret) {
            Ok(fd) => {
                if let Err(_errno) = sys::close(fd as i32) {}
            }
            Err(errno) => crate::abort!("frame probe openat failed: errno {errno}"),
        }
    }));
    largest = largest.max(measure(region, "readlinkat", || {
        let mut buffer = [0u8; 256];
        let ret = unsafe {
            stub(
                sys::AT_FDCWD as usize,
                c"/proc/self/exe".as_ptr() as usize,
                buffer.as_mut_ptr() as usize,
                buffer.len(),
                0,
                0,
                nr::READLINKAT as usize,
            )
        };
        if ret < 0 {
            crate::abort!("frame probe readlinkat failed: {ret}");
        }
    }));
    largest = largest.max(measure(region, "getcwd", || {
        let mut buffer = [0u8; 256];
        let ret = unsafe { stub(buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0, 0, nr::GETCWD as usize) };
        if ret < 0 {
            crate::abort!("frame probe getcwd failed: {ret}");
        }
    }));
    largest = largest.max(measure(region, "renameat", || {
        let ret = unsafe {
            stub(
                sys::AT_FDCWD as usize,
                c"/nonexistent-zd-selftest-a".as_ptr() as usize,
                sys::AT_FDCWD as usize,
                c"/nonexistent-zd-selftest-b".as_ptr() as usize,
                0,
                0,
                nr::RENAMEAT as usize,
            )
        };
        if sys::check(ret) != Err(sys::ENOENT) {
            crate::abort!("frame probe renameat: expected ENOENT, got {ret}");
        }
    }));
    largest = largest.max(measure(region, "symlinkat", || {
        let ret = unsafe {
            stub(
                c"zd-selftest-target".as_ptr() as usize,
                sys::AT_FDCWD as usize,
                c"zd-nonexistent-dir/zd-selftest-link".as_ptr() as usize,
                0,
                0,
                0,
                nr::SYMLINKAT as usize,
            )
        };
        if sys::check(ret) != Err(sys::ENOENT) {
            crate::abort!("frame probe symlinkat: expected ENOENT, got {ret}");
        }
    }));
    largest = largest.max(measure(region, "linkat_copy", || {
        let copy = c"/tmp/zd-selftest-copy";
        let ret = unsafe {
            stub(
                sys::AT_FDCWD as usize,
                guest.as_ptr() as usize,
                sys::AT_FDCWD as usize,
                copy.as_ptr() as usize,
                0,
                0,
                nr::LINKAT as usize,
            )
        };
        if let Err(errno) = sys::check(ret) {
            emit_line!("zd-selftest: frame kind=linkat_copy link_errno={errno}");
        }
        let ret = unsafe { stub(sys::AT_FDCWD as usize, copy.as_ptr() as usize, 0, 0, 0, 0, nr::UNLINKAT as usize) };
        if let Err(errno) = sys::check(ret) {
            emit_line!("zd-selftest: frame kind=linkat_copy unlink_errno={errno}");
        }
    }));
    largest = largest.max(measure(region, "bind", || {
        let fd = match sys::check(unsafe { sys::syscall6(nr::SOCKET, 1, 1 | sys::O_CLOEXEC, 0, 0, 0, 0) }) {
            Ok(fd) => fd as i32,
            Err(errno) => {
                emit_line!("zd-selftest: frame kind=bind skipped=socket errno {errno}");
                return;
            }
        };
        let path = b"/nonexistent-zd-dir/zd-selftest-sock";
        let mut address = [0u8; 110];
        address[0] = 1;
        address[2..2 + path.len()].copy_from_slice(path);
        let ret = unsafe { stub(fd as usize, address.as_ptr() as usize, 2 + path.len() + 1, 0, 0, 0, nr::BIND as usize) };
        if let Err(_errno) = sys::close(fd) {}
        if sys::check(ret) != Err(sys::ENOENT) {
            crate::abort!("frame probe bind: expected ENOENT, got {ret}");
        }
    }));
    largest = largest.max(measure(region, "execve", || {
        // SAFETY: single-threaded startup; the loader path is put back
        // before anything else can read it.
        let config = unsafe { config::get_mut() };
        let mut original = [0u8; MAX_PATH];
        let original_len = config.loader_path.as_bytes().len();
        original[..original_len].copy_from_slice(config.loader_path.as_bytes());
        if config.loader_path.set(b"/nonexistent-zd-selftest-loader").is_err() {
            return;
        }
        let argv = [c"zd-selftest".as_ptr().cast::<u8>(), core::ptr::null()];
        let envp = [c"ZD_SELFTEST_PROBE=1".as_ptr().cast::<u8>(), core::ptr::null()];
        let ret = unsafe {
            stub(
                guest.as_ptr() as usize,
                argv.as_ptr() as usize,
                envp.as_ptr() as usize,
                0,
                0,
                0,
                nr::EXECVE as usize,
            )
        };
        if config.loader_path.set(&original[..original_len]).is_err() {
            crate::abort!("could not restore the loader path after the exec probe");
        }
        if sys::check(ret) != Err(sys::ENOENT) {
            crate::abort!("frame probe execve: expected ENOENT, got {ret}");
        }
    }));
    emit_line!("zd-selftest: frame max={largest} budget={FRAME_BUDGET}");
    // SAFETY: single-threaded startup; read back by `report_load`.
    unsafe { *FRAME_MAX.get() = largest };
    if let Err(_errno) = sys::munmap(stack, PROBE_STACK) {
        // The probe stack stays mapped; the guest never sees it.
    }
}

fn measure(region: &mut [u8], name: &str, operation: impl FnOnce()) -> usize {
    region.fill(FILL);
    let probe = sys::SigAltStack {
        sp: region.as_ptr() as u64,
        flags: 0,
        size: region.len() as u64,
    };
    if let Err(errno) = sys::sigaltstack(Some(&probe), None) {
        emit_line!("zd-selftest: frame kind={name} skipped=sigaltstack errno {errno}");
        return 0;
    }
    operation();
    let disabled = sys::SigAltStack {
        sp: 0,
        flags: sys::SS_DISABLE,
        size: 0,
    };
    if let Err(errno) = sys::sigaltstack(Some(&disabled), None) {
        crate::abort!("sigaltstack(SS_DISABLE) failed: errno {errno}");
    }
    let untouched = region.iter().position(|&byte| byte != FILL).unwrap_or(region.len());
    let used = region.len() - untouched;
    emit_line!("zd-selftest: frame kind={name} bytes={used}");
    used
}

pub fn report_load(program: &[u8], loaded: &Loaded, started_ns: u64) {
    // SAFETY: written once in `preamble`, before any thread exists.
    if let Some(stub) = unsafe { *STUB.get() } {
        let mut buffer = [0u8; 256];
        // SAFETY: a trapped readlinkat issued the way guest code would.
        let ret = unsafe {
            stub(
                sys::AT_FDCWD as usize,
                c"/proc/self/exe".as_ptr() as usize,
                buffer.as_mut_ptr() as usize,
                buffer.len(),
                0,
                0,
                nr::READLINKAT as usize,
            )
        };
        let shown = if ret > 0 { &buffer[..(ret as usize).min(buffer.len())] } else { &buffer[..0] };
        emit_line!("zd-selftest: proc_exe ret={ret} value={}", Bytes(shown));
    }
    emit_line!(
        "zd-selftest: load exe={} base={:#x} entry={:#x} phdr={:#x} interp={} interp_base={:#x} us={}",
        Bytes(program),
        loaded.base,
        loaded.entry,
        loaded.phdr,
        Bytes(loaded.interp_path()),
        loaded.interp_base,
        (now_ns() - started_ns) / 1000
    );
    // SAFETY: written once in `frame_probe`, before any thread exists.
    let frame_max = unsafe { *FRAME_MAX.get() };
    let result = if frame_max <= FRAME_BUDGET { "PASS" } else { "FAIL" };
    emit_line!(
        "zd-selftest: result={result} traps={} compat_traps={} frame_max={frame_max}",
        TRAP_COUNT.load(core::sync::atomic::Ordering::Relaxed),
        COMPAT_COUNT.load(core::sync::atomic::Ordering::Relaxed)
    );
}
