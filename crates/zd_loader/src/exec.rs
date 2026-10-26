//! `execve` and `execveat` from the trap handler.
//!
//! The kernel cannot run a guest binary itself (its `PT_INTERP` is a
//! guest path and, on this platform, app files are not exec'able), so
//! every exec becomes an exec of the loader with the guest's argv
//! untouched and the guest path in `ZD_PROGRAM`. The failures a guest
//! expects from `execve` (`ENOENT`, `EACCES`, `ENOEXEC`) are produced
//! here beforehand; once the loader replaces the image nothing can be
//! reported back.

use zd_engine_core::elf::{EM_AARCH64, HEADER_SIZE};
use zd_engine_core::env;
use zd_engine_core::identity::MAX_ENCODED;
use zd_engine_core::path::{MAX_PATH, Writer, is_absolute};
use zd_engine_core::shebang;
use zd_engine_core::stack::env_key;
use zd_engine_core::sysno::nr;
use zd_engine_core::translate::{ProcExe, Translated, Translator};

use crate::config::{self, ENGINE_KEYS};
use crate::handler::cstr_bounded;
use crate::log;
use crate::sys;

/// Environment pointer slots kept on the handler's stack; larger
/// environments get an anonymous mapping instead.
const STACK_ENTRIES: usize = 256;

pub fn handle(number: u32, regs: &mut [u64; 31]) {
    let (dirfd, path, argv, envp, flags) = if number == nr::EXECVE {
        (sys::AT_FDCWD, regs[0], regs[1], regs[2], 0usize)
    } else {
        (regs[0] as isize, regs[1], regs[2], regs[3], regs[4] as usize)
    };
    regs[0] = run(
        dirfd,
        path as *const u8,
        argv as *const *const u8,
        envp as *const *const u8,
        flags,
    );
}

#[inline(never)]
fn run(dirfd: isize, path: *const u8, argv: *const *const u8, envp: *const *const u8, flags: usize) -> u64 {
    let Some(translator) = config::translator() else {
        return sys::neg_errno(sys::ENOSYS);
    };
    let Some(given) = (unsafe { cstr_bounded(path, MAX_PATH) }) else {
        return sys::neg_errno(if path.is_null() { sys::EFAULT } else { sys::ENAMETOOLONG });
    };

    // `guest` is what the child's loader will open (and record as the
    // guest's executable); `host` is what the preflight checks here.
    let mut guest = [0u8; MAX_PATH];
    let mut host = [0u8; MAX_PATH];
    let guest_len = match resolve(translator, dirfd, given, flags, &mut guest, &mut host) {
        Ok(len) => len,
        Err(errno) => return sys::neg_errno(errno),
    };
    if let Err(errno) = preflight(translator, host.as_ptr()) {
        return sys::neg_errno(errno);
    }

    let envc = count_pointers(envp);
    let needed = envc + ENGINE_KEYS.len() + 3;
    let mut on_stack = [core::ptr::null::<u8>(); STACK_ENTRIES];
    let mut mapped: Option<(usize, usize)> = None;
    let slots: &mut [*const u8] = if needed <= STACK_ENTRIES {
        &mut on_stack[..needed]
    } else {
        let page = config::get().page_size.max(4096);
        let len = (needed * 8).div_ceil(page) * page;
        match sys::mmap(0, len, sys::PROT_READ | sys::PROT_WRITE, sys::MAP_PRIVATE | sys::MAP_ANONYMOUS, -1, 0) {
            Ok(address) => {
                mapped = Some((address, len));
                // SAFETY: a fresh zeroed mapping of at least `needed` slots.
                unsafe { core::slice::from_raw_parts_mut(address as *mut *const u8, needed) }
            }
            Err(errno) => return sys::neg_errno(errno),
        }
    };

    let config = config::get();
    let mut count = 0usize;
    let mut present = [false; ENGINE_KEYS.len()];
    for index in 0..envc {
        // SAFETY: `envp` has `envc` non-null entries, as handed to the kernel.
        let entry = unsafe { *envp.add(index) };
        let Some(bytes) = (unsafe { cstr_bounded(entry, 1 << 17) }) else {
            continue;
        };
        let key = env_key(bytes);
        if env::PER_EXEC.iter().any(|name| name.as_bytes() == key) {
            continue;
        }
        for (slot, name) in present.iter_mut().zip(ENGINE_KEYS.iter()) {
            if name.as_bytes() == key {
                *slot = true;
            }
        }
        slots[count] = entry;
        count += 1;
    }
    for (index, is_present) in present.iter().enumerate() {
        if !is_present && !config.engine_entries[index].is_empty() {
            slots[count] = config.engine_entries[index].as_ptr();
            count += 1;
        }
    }
    let mut program_entry = [0u8; MAX_PATH + 16];
    {
        let mut writer = Writer::new(&mut program_entry);
        if writer.push(env::PROGRAM.as_bytes()).is_err()
            || writer.push(b"=").is_err()
            || writer.push(&guest[..guest_len]).is_err()
        {
            release(mapped);
            return sys::neg_errno(sys::ENAMETOOLONG);
        }
        writer.finish();
    }
    slots[count] = program_entry.as_ptr();
    count += 1;
    let mut identity_entry = [0u8; 16 + MAX_ENCODED];
    if config.fake_root {
        let mut encoded = [0u8; MAX_ENCODED];
        let encoded_len = config.identity.encode(&mut encoded);
        let mut writer = Writer::new(&mut identity_entry);
        if writer.push(env::IDENTITY.as_bytes()).is_err()
            || writer.push(b"=").is_err()
            || writer.push(&encoded[..encoded_len]).is_err()
        {
            release(mapped);
            return sys::neg_errno(sys::ENAMETOOLONG);
        }
        writer.finish();
        slots[count] = identity_entry.as_ptr();
        count += 1;
    }
    slots[count] = core::ptr::null();

    let errno = sys::execve(config.loader_path.as_ptr(), argv, slots.as_ptr());
    release(mapped);
    sys::neg_errno(errno)
}

fn release(mapped: Option<(usize, usize)>) {
    if let Some((address, len)) = mapped
        && let Err(_errno) = sys::munmap(address, len)
    {
        // Leaked pages in a process that just failed to exec.
    }
}

fn count_pointers(list: *const *const u8) -> usize {
    if list.is_null() {
        return 0;
    }
    let mut count = 0usize;
    // SAFETY: a NULL-terminated guest pointer array, as handed to the kernel.
    while !unsafe { *list.add(count) }.is_null() {
        count += 1;
    }
    count
}

/// Work out the guest path the child loads and the host path to check.
fn resolve(
    translator: &Translator<'static>,
    dirfd: isize,
    given: &[u8],
    flags: usize,
    guest: &mut [u8; MAX_PATH],
    host: &mut [u8; MAX_PATH],
) -> Result<usize, i32> {
    if given.is_empty() {
        if flags & sys::AT_EMPTY_PATH == 0 {
            return Err(sys::ENOENT);
        }
        // fexecve: the kernel knows the file behind the fd.
        let len = readlink_fd(dirfd, host)?;
        if host[..len].ends_with(b" (deleted)") {
            return Err(sys::ENOENT);
        }
        let guest_len = translator.host_to_guest(&host[..len], guest).map_err(|_| sys::ENAMETOOLONG)?;
        return Ok(guest_len);
    }
    if !is_absolute(given) && dirfd != sys::AT_FDCWD {
        // Relative to a directory fd: anchor on the directory's guest path.
        let mut dir_host = [0u8; MAX_PATH];
        let dir_len = readlink_fd(dirfd, &mut dir_host)?;
        let mut dir_guest = [0u8; MAX_PATH];
        let dir_guest_len = translator
            .host_to_guest(&dir_host[..dir_len], &mut dir_guest)
            .map_err(|_| sys::ENAMETOOLONG)?;
        let guest_len = {
            let mut writer = Writer::new(guest);
            writer.push(&dir_guest[..dir_guest_len]).map_err(|_| sys::ENAMETOOLONG)?;
            writer.push_component(given).map_err(|_| sys::ENAMETOOLONG)?;
            writer.finish()
        };
        match translator.guest_to_host(&guest[..guest_len], host) {
            Ok(Translated::Host(_)) => Ok(guest_len),
            _ => Err(sys::ENOENT),
        }
    } else {
        match translator.guest_to_host(given, host) {
            Ok(Translated::Host(_)) => copy_guest(given, guest),
            Ok(Translated::Relative) => {
                let len = copy_guest(given, guest)?;
                host[..len].copy_from_slice(given);
                host[len] = 0;
                Ok(len)
            }
            Ok(Translated::ProcExe(who)) if is_self(who) => {
                let guest_exe = config::get().guest_exe.as_bytes();
                let len = copy_guest(guest_exe, guest)?;
                match translator.guest_to_host(guest_exe, host) {
                    Ok(Translated::Host(_)) => {}
                    Ok(Translated::Relative) => {
                        host[..len].copy_from_slice(guest_exe);
                        host[len] = 0;
                    }
                    _ => return Err(sys::ENOENT),
                }
                Ok(len)
            }
            Ok(_) => Err(sys::ENOENT),
            Err(_) => Err(sys::ENAMETOOLONG),
        }
    }
}

fn is_self(who: ProcExe) -> bool {
    match who {
        ProcExe::SelfProcess | ProcExe::ThreadSelf => true,
        ProcExe::Pid(pid) => pid == sys::getpid(),
    }
}

fn copy_guest(from: &[u8], guest: &mut [u8; MAX_PATH]) -> Result<usize, i32> {
    if from.len() >= MAX_PATH {
        return Err(sys::ENAMETOOLONG);
    }
    guest[..from.len()].copy_from_slice(from);
    guest[from.len()] = 0;
    Ok(from.len())
}

fn readlink_fd(fd: isize, out: &mut [u8; MAX_PATH]) -> Result<usize, i32> {
    let mut link = [0u8; 32];
    let mut writer = Writer::new(&mut link);
    let mut digits = [0u8; 20];
    if writer.push(b"/proc/self/fd/").is_err() || writer.push(log::itoa(&mut digits, fd as u64)).is_err() {
        return Err(sys::EBADF);
    }
    writer.finish();
    let len = sys::readlinkat(sys::AT_FDCWD, link.as_ptr(), out)?;
    if len >= MAX_PATH {
        return Err(sys::ENAMETOOLONG);
    }
    out[len] = 0;
    Ok(len)
}

/// The checks the kernel would do before committing to the exec.
fn preflight(translator: &Translator<'static>, host: *const u8) -> Result<(), i32> {
    let mode = sys::stat_mode(sys::AT_FDCWD, host, 0)?;
    if mode & sys::S_IFMT != sys::S_IFREG {
        return Err(sys::EACCES);
    }
    sys::faccessat(sys::AT_FDCWD, host, sys::X_OK).map_err(|_| sys::EACCES)?;
    let fd = sys::openat(sys::AT_FDCWD, host, sys::O_RDONLY | sys::O_CLOEXEC, 0)?;
    let mut head = [0u8; shebang::LINE_LIMIT];
    let read = sys::read(fd, &mut head);
    if let Err(_errno) = sys::close(fd) {}
    let len = read?;
    let head = &head[..len];
    if head.starts_with(b"#!") {
        let Some(script) = shebang::parse(head) else {
            return Err(sys::ENOEXEC);
        };
        let mut interpreter = [0u8; MAX_PATH];
        return match translator.guest_to_host(script.interpreter, &mut interpreter) {
            Ok(Translated::Host(_)) => {
                sys::faccessat(sys::AT_FDCWD, interpreter.as_ptr(), sys::X_OK).map_err(|_| sys::ENOENT)
            }
            Ok(Translated::Relative) => Ok(()),
            _ => Err(sys::ENOENT),
        };
    }
    if len >= HEADER_SIZE
        && head[..4] == [0x7f, b'E', b'L', b'F']
        && head[4] == 2
        && u16::from_le_bytes([head[18], head[19]]) == EM_AARCH64
    {
        return Ok(());
    }
    Err(sys::ENOEXEC)
}
