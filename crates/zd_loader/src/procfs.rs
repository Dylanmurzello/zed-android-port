//! The `/proc` files the Android app sandbox refuses, served by the engine.
//!
//! An untrusted app may read `/proc/cpuinfo`, `/proc/meminfo`,
//! `/proc/mounts` and its own `/proc/self/*`, and nothing else under
//! `/proc` (measured on Android 16: `stat`, `loadavg`, `uptime`,
//! `version`, `vmstat`, `diskstats`, `swaps`, `filesystems`, `net/*`
//! and the `sys/*` knobs all answer EACCES on open and are invisible to
//! stat). Linux software takes those files for granted: libuv counts
//! the `cpu<n>` lines of `/proc/stat` for `os.cpus()`, and npm sizes its
//! script queue from that count, so a zero made every background
//! lifecycle script a silent no-op. The engine answers from what the
//! sandbox allows: `sysinfo(2)`, the clocks, `uname(2)` and the cpu
//! `online` list under `/sys`. Counters it cannot see are zero.
//!
//! The kernel's own file is always tried first, so a device or release
//! that allows one of these keeps the real file.

use zd_engine_core::procfs::{SynthFile, constant_content, cpu_list};
use zd_engine_core::sysno::nr;

use crate::config;
use crate::handler::{open_how_flags, serve_bytes};
use crate::log;
use crate::sys;

const MAX_CPUS: usize = 1024;
/// Inode numbers of the served files, one block apart from anything the
/// kernel hands out for `/proc`.
const INODE_BASE: u64 = 0x5a44_0000;

/// Answer a path syscall on a served file after the kernel refused its
/// own; `result` is what the kernel said and stands unless it refused.
pub(crate) fn serve(
    file: SynthFile,
    number: u32,
    regs: &[u64; 31],
    result: u64,
    render_buffer: &mut [u8],
) -> u64 {
    let refused = result == sys::neg_errno(sys::EACCES) || result == sys::neg_errno(sys::ENOENT);
    if !refused {
        return result;
    }
    match number {
        nr::OPENAT => open(file, regs[2] as usize, render_buffer),
        nr::OPENAT2 => open(file, open_how_flags(regs[2] as *const u64), render_buffer),
        nr::NEWFSTATAT => {
            write_stat(file, regs[2] as *mut u8);
            0
        }
        nr::STATX => {
            write_statx(file, regs[4] as *mut u8);
            0
        }
        nr::FACCESSAT | nr::FACCESSAT2 => {
            if regs[2] as usize & (sys::W_OK | sys::X_OK) != 0 {
                sys::neg_errno(sys::EACCES)
            } else {
                0
            }
        }
        nr::READLINKAT => sys::neg_errno(sys::EINVAL),
        _ => result,
    }
}

fn open(file: SynthFile, flags: usize, render_buffer: &mut [u8]) -> u64 {
    if flags & sys::O_DIRECTORY != 0 {
        return sys::neg_errno(sys::ENOTDIR);
    }
    if flags & sys::O_ACCMODE != sys::O_RDONLY {
        return sys::neg_errno(sys::EACCES);
    }
    let len = render(file, render_buffer);
    serve_bytes(c"proc", &render_buffer[..len], flags)
}

/// The file's text, truncated to the buffer.
pub(crate) fn render(file: SynthFile, out: &mut [u8]) -> usize {
    if let Some(bytes) = constant_content(file) {
        let len = bytes.len().min(out.len());
        out[..len].copy_from_slice(&bytes[..len]);
        return len;
    }
    let mut text = Text { out, len: 0 };
    match file {
        SynthFile::Stat => render_stat(&mut text),
        SynthFile::Loadavg => render_loadavg(&mut text),
        SynthFile::Uptime => render_uptime(&mut text),
        SynthFile::Vmstat => render_vmstat(&mut text),
        SynthFile::Version => {
            let utsname = uname_view();
            text.push(b"Linux version ");
            text.push(field(&utsname, 2));
            text.push(b" (zdroid@android) (unknown) ");
            text.push(field(&utsname, 3));
            text.push(b"\n");
        }
        SynthFile::SysOsRelease => text.line(field(&uname_view(), 2)),
        SynthFile::SysHostname => text.line(field(&uname_view(), 1)),
        SynthFile::SysVersion => text.line(field(&uname_view(), 3)),
        _ => {}
    }
    text.len
}

struct Text<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Text<'_> {
    fn push(&mut self, bytes: &[u8]) {
        let room = self.out.len() - self.len;
        let count = bytes.len().min(room);
        self.out[self.len..self.len + count].copy_from_slice(&bytes[..count]);
        self.len += count;
    }

    fn line(&mut self, bytes: &[u8]) {
        self.push(bytes);
        self.push(b"\n");
    }

    fn number(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let text = log::itoa(&mut digits, value);
        self.push(text);
    }

    /// A 16.16 fixed-point value with two decimals, the load average's
    /// form.
    fn fixed_hundredths(&mut self, value: u64) {
        let mut whole = value >> 16;
        let mut hundredths = ((value & 0xffff) * 100 + 0x8000) >> 16;
        if hundredths == 100 {
            whole += 1;
            hundredths = 0;
        }
        self.number(whole);
        self.push(b".");
        if hundredths < 10 {
            self.push(b"0");
        }
        self.number(hundredths);
    }
}

fn render_stat(text: &mut Text<'_>) {
    text.push(b"cpu  0 0 0 0 0 0 0 0 0 0\n");
    let mut online = [0u8; 256];
    let online_len = read_file(c"/sys/devices/system/cpu/online", &mut online);
    let mut count = 0usize;
    for id in cpu_list(&online[..online_len]).take(MAX_CPUS) {
        text.push(b"cpu");
        text.number(u64::from(id));
        text.push(b" 0 0 0 0 0 0 0 0 0 0\n");
        count += 1;
    }
    if count == 0 {
        text.push(b"cpu0 0 0 0 0 0 0 0 0 0 0\n");
    }
    let info = system_info();
    text.push(b"intr 0\nctxt 0\nbtime ");
    text.number(boot_time());
    text.push(b"\nprocesses ");
    text.number(u64::from(info.procs));
    text.push(b"\nprocs_running 1\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n");
}

fn render_loadavg(text: &mut Text<'_>) {
    let info = system_info();
    for (index, load) in info.loads.iter().enumerate() {
        if index > 0 {
            text.push(b" ");
        }
        text.fixed_hundredths(*load);
    }
    text.push(b" 1/");
    text.number(u64::from(info.procs.max(1)));
    text.push(b" ");
    text.number(u64::from(sys::getpid()));
    text.push(b"\n");
}

fn render_uptime(text: &mut Text<'_>) {
    let info = system_info();
    let uptime = info.uptime.max(0) as u64;
    let mut online = [0u8; 256];
    let online_len = read_file(c"/sys/devices/system/cpu/online", &mut online);
    let cpus = cpu_list(&online[..online_len])
        .take(MAX_CPUS)
        .count()
        .max(1) as u64;
    text.number(uptime);
    text.push(b".00 ");
    text.number(uptime.saturating_mul(cpus));
    text.push(b".00\n");
}

fn render_vmstat(text: &mut Text<'_>) {
    let info = system_info();
    let page = config::get().page_size.max(4096) as u64;
    let unit = u64::from(info.mem_unit.max(1));
    let pages = |bytes: u64| bytes.saturating_mul(unit) / page;
    text.push(b"nr_free_pages ");
    text.number(pages(info.freeram));
    text.push(b"\n");
    for name in [
        &b"nr_zone_inactive_anon"[..],
        b"nr_zone_active_anon",
        b"nr_zone_inactive_file",
        b"nr_zone_active_file",
        b"nr_zone_unevictable",
        b"nr_inactive_anon",
        b"nr_active_anon",
        b"nr_inactive_file",
        b"nr_active_file",
        b"nr_unevictable",
        b"nr_mlock",
        b"nr_anon_pages",
        b"nr_mapped",
        b"nr_file_pages",
        b"nr_dirty",
        b"nr_writeback",
    ] {
        text.push(name);
        text.push(b" 0\n");
    }
    text.push(b"nr_shmem ");
    text.number(pages(info.sharedram));
    text.push(b"\n");
    for name in [
        &b"nr_slab_reclaimable"[..],
        b"nr_slab_unreclaimable",
        b"pgpgin",
        b"pgpgout",
        b"pswpin",
        b"pswpout",
        b"pgfault",
        b"pgmajfault",
        b"pgfree",
        b"oom_kill",
    ] {
        text.push(name);
        text.push(b" 0\n");
    }
}

struct SystemInfo {
    uptime: i64,
    loads: [u64; 3],
    freeram: u64,
    sharedram: u64,
    procs: u16,
    mem_unit: u32,
}

/// `sysinfo(2)`, zeros when the kernel refuses it.
fn system_info() -> SystemInfo {
    let mut buffer = [0u8; 112];
    if let Err(_errno) = sys::sysinfo(&mut buffer) {
        buffer = [0; 112];
    }
    let word =
        |offset: usize| u64::from_ne_bytes(buffer[offset..offset + 8].try_into().unwrap_or([0; 8]));
    SystemInfo {
        uptime: word(0) as i64,
        loads: [word(8), word(16), word(24)],
        freeram: word(40),
        sharedram: word(48),
        procs: u16::from_ne_bytes([buffer[80], buffer[81]]),
        mem_unit: u32::from_ne_bytes(buffer[104..108].try_into().unwrap_or([0; 4])),
    }
}

/// Seconds since the epoch at boot: the realtime clock less the boot clock.
fn boot_time() -> u64 {
    let now = sys::clock_gettime(sys::CLOCK_REALTIME).map_or(0, |(seconds, _)| seconds);
    let since_boot = sys::clock_gettime(sys::CLOCK_BOOTTIME).map_or(0, |(seconds, _)| seconds);
    now.saturating_sub(since_boot).max(0) as u64
}

/// `uname(2)` as the guest sees it: with the configured node name and
/// release, like the trapped syscall answers.
fn uname_view() -> [u8; 390] {
    let mut buffer = [0u8; 390];
    if let Err(_errno) = sys::uname(&mut buffer) {
        buffer[..5].copy_from_slice(b"Linux");
    }
    let config = config::get();
    for (index, value) in [(1usize, &config.hostname), (2usize, &config.uname_release)] {
        if value.is_empty() {
            continue;
        }
        let bytes = value.as_bytes();
        let len = bytes.len().min(64);
        let field = &mut buffer[index * 65..(index + 1) * 65];
        field[..len].copy_from_slice(&bytes[..len]);
        field[len..].fill(0);
    }
    buffer
}

fn field(utsname: &[u8; 390], index: usize) -> &[u8] {
    let field = &utsname[index * 65..(index + 1) * 65];
    let len = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    &field[..len]
}

fn read_file(path: &core::ffi::CStr, buffer: &mut [u8]) -> usize {
    let Ok(fd) = sys::openat(
        sys::AT_FDCWD,
        path.as_ptr().cast(),
        sys::O_RDONLY | sys::O_CLOEXEC,
        0,
    ) else {
        return 0;
    };
    let mut filled = 0usize;
    while filled < buffer.len() {
        match sys::read(fd, &mut buffer[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => filled += count,
        }
    }
    if let Err(_errno) = sys::close(fd) {}
    filled
}

fn inode(file: SynthFile) -> u64 {
    INODE_BASE + file as u64
}

fn now() -> (i64, i64) {
    sys::clock_gettime(sys::CLOCK_REALTIME).unwrap_or((0, 0))
}

/// `struct stat` of a served file: a root-owned, read-only regular file
/// of size zero, as procfs reports its own.
fn write_stat(file: SynthFile, buffer: *mut u8) {
    if buffer.is_null() {
        return;
    }
    let (seconds, nanoseconds) = now();
    let mut bytes = [0u8; 128];
    bytes[8..16].copy_from_slice(&inode(file).to_ne_bytes());
    bytes[16..20].copy_from_slice(&(sys::S_IFREG | 0o444).to_ne_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_ne_bytes());
    bytes[56..60].copy_from_slice(&1024i32.to_ne_bytes());
    for offset in [72usize, 88, 104] {
        bytes[offset..offset + 8].copy_from_slice(&seconds.to_ne_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&nanoseconds.to_ne_bytes());
    }
    // SAFETY: the guest's stat buffer, as handed to the kernel.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len()) };
}

/// `struct statx` of a served file, basic stats only.
fn write_statx(file: SynthFile, buffer: *mut u8) {
    if buffer.is_null() {
        return;
    }
    const STATX_BASIC_STATS: u32 = 0x7ff;
    let (seconds, nanoseconds) = now();
    let mut bytes = [0u8; 256];
    bytes[0..4].copy_from_slice(&STATX_BASIC_STATS.to_ne_bytes());
    bytes[4..8].copy_from_slice(&1024u32.to_ne_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_ne_bytes());
    bytes[28..30].copy_from_slice(&((sys::S_IFREG | 0o444) as u16).to_ne_bytes());
    bytes[32..40].copy_from_slice(&inode(file).to_ne_bytes());
    for offset in [64usize, 80, 96, 112] {
        bytes[offset..offset + 8].copy_from_slice(&seconds.to_ne_bytes());
        bytes[offset + 8..offset + 12].copy_from_slice(&(nanoseconds as u32).to_ne_bytes());
    }
    // SAFETY: the guest's statx buffer, as handed to the kernel.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len()) };
}
