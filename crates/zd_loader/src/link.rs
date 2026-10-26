//! `linkat` without hard links.
//!
//! The app sandbox refuses `link(2)` on app data: on this device the
//! kernel answers EACCES for the app's own files in every domain tried,
//! and the installer meets the same refusal when it extracts the rootfs.
//! Hard links still matter inside: dpkg backs its database up with one
//! before every write, packages carry hard-linked members, cargo links
//! build artefacts into place. When the kernel refuses, the new name
//! becomes a copy with the same content, mode and timestamps and its own
//! inode. Writes through one name then no longer show through the other,
//! which none of those users rely on.

use zd_engine_core::path::{MAX_PATH, Writer};

use crate::config::Global;
use crate::load::close_quietly;
use crate::log;
use crate::sys;

const EIO: i32 = 5;

/// Link counts this process would observe for the copies it made. shadow's
/// lock protocol (`useradd`, `groupadd`, `passwd`, `chpasswd`) links its pid
/// file to the lock file and then requires `st_nlink == 2` on the source
/// before it trusts the lock. The table is process-local, never
/// decremented and tiny: a stale entry can only inflate the count of a
/// later file that reuses the inode number, inside this process.
struct Counts {
    entries: [(u64, u32); SLOTS],
    next: usize,
}

const SLOTS: usize = 32;

static COUNTS: Global<Counts> = Global::new(Counts {
    entries: [(0, 0); SLOTS],
    next: 0,
});

impl Counts {
    fn slot(&mut self, ino: u64) -> Option<&mut (u64, u32)> {
        self.entries.iter_mut().find(|entry| entry.0 == ino)
    }

    fn insert(&mut self, ino: u64, extra: u32) {
        self.entries[self.next] = (ino, extra);
        self.next = (self.next + 1) % SLOTS;
    }

    /// One more link for `source`; the copy starts with the same total.
    fn record(&mut self, source: u64, copy: u64) {
        let extra = match self.slot(source) {
            Some(entry) => {
                entry.1 += 1;
                entry.1
            }
            None => {
                self.insert(source, 1);
                1
            }
        };
        match self.slot(copy) {
            Some(entry) => entry.1 = extra,
            None => self.insert(copy, extra),
        }
    }
}

/// Links to add to the kernel's `st_nlink` for inode `ino`.
pub fn extra_links(ino: u64) -> u32 {
    if ino == 0 {
        return 0;
    }
    // SAFETY: handler-only access; a racing handler on another thread can
    // at worst miscount by one.
    let counts = unsafe { &*COUNTS.get() };
    counts
        .entries
        .iter()
        .find(|entry| entry.0 == ino)
        .map_or(0, |entry| entry.1)
}

pub fn by_copy(old_dirfd: isize, old: *const u8, new_dirfd: isize, new: *const u8, flags: usize) -> u64 {
    match copy(old_dirfd, old, new_dirfd, new, flags) {
        Ok(()) => 0,
        Err(errno) => sys::neg_errno(errno),
    }
}

fn copy(old_dirfd: isize, old: *const u8, new_dirfd: isize, new: *const u8, flags: usize) -> Result<(), i32> {
    // SAFETY: the kernel just read this path; it is NUL-terminated.
    let empty = flags & sys::AT_EMPTY_PATH != 0 && unsafe { *old } == 0;
    let follow = flags & sys::AT_SYMLINK_FOLLOW != 0;
    let source = if empty {
        reopen(old_dirfd as i32)?
    } else {
        let mut stat = [0u8; 128];
        let stat_flags = if follow { 0 } else { sys::AT_SYMLINK_NOFOLLOW };
        sys::fstatat_raw(old_dirfd, old, stat_flags, &mut stat)?;
        let mode = word(&stat, 16);
        // Only a regular file is opened: opening a fifo for reading would
        // wait for a writer, and the rest cannot be copied.
        match mode & sys::S_IFMT {
            sys::S_IFLNK => return copy_symlink(old_dirfd, old, new_dirfd, new),
            sys::S_IFIFO => return sys::mknodat(new_dirfd, new, sys::S_IFIFO | (mode & 0o7777), 0),
            sys::S_IFREG => {}
            _ => return Err(sys::EPERM),
        }
        let open_flags = if follow { 0 } else { sys::O_NOFOLLOW };
        sys::openat(old_dirfd, old, sys::O_RDONLY | sys::O_CLOEXEC | open_flags, 0)?
    };
    let result = copy_open(source, new_dirfd, new);
    close_quietly(source);
    result
}

/// The file behind `fd`, reopened from the start through procfs.
fn reopen(fd: i32) -> Result<i32, i32> {
    let mut link = [0u8; 32];
    let mut writer = Writer::new(&mut link);
    let mut digits = [0u8; 20];
    if writer.push(b"/proc/self/fd/").is_err() || writer.push(log::itoa(&mut digits, fd as u64)).is_err() {
        return Err(sys::EINVAL);
    }
    writer.finish();
    sys::openat(sys::AT_FDCWD, link.as_ptr(), sys::O_RDONLY | sys::O_CLOEXEC, 0)
}

fn copy_symlink(old_dirfd: isize, old: *const u8, new_dirfd: isize, new: *const u8) -> Result<(), i32> {
    let mut target = [0u8; MAX_PATH];
    let len = sys::readlinkat(old_dirfd, old, &mut target[..MAX_PATH - 1])?;
    target[len] = 0;
    sys::symlinkat(target.as_ptr(), new_dirfd, new)
}

fn copy_open(source: i32, new_dirfd: isize, new: *const u8) -> Result<(), i32> {
    let mut stat = [0u8; 128];
    sys::fstat_raw(source, &mut stat)?;
    let mode = word(&stat, 16);
    let permissions = mode & 0o7777;
    if mode & sys::S_IFMT != sys::S_IFREG {
        return Err(sys::EPERM);
    }
    let dest = sys::openat(
        new_dirfd,
        new,
        sys::O_WRONLY | sys::O_CREAT | sys::O_EXCL | sys::O_CLOEXEC,
        permissions as usize,
    )?;
    let times = [long(&stat, 72), long(&stat, 80), long(&stat, 88), long(&stat, 96)];
    let result = copy_contents(source, dest)
        .and_then(|()| sys::fchmod(dest, permissions))
        .and_then(|()| sys::futimens(dest, &times))
        .and_then(|()| {
            let mut copy_stat = [0u8; 128];
            sys::fstat_raw(dest, &mut copy_stat)?;
            // SAFETY: see `extra_links`.
            unsafe { &mut *COUNTS.get() }.record(long(&stat, 8), long(&copy_stat, 8));
            Ok(())
        });
    close_quietly(dest);
    if result.is_err() && let Err(_errno) = sys::unlinkat(new_dirfd, new, 0) {
        // The failed copy stays behind; the caller gets the copy error.
    }
    result
}

fn copy_contents(source: i32, dest: i32) -> Result<(), i32> {
    loop {
        match sys::sendfile(dest, source, 1 << 20) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(sys::EINVAL) | Err(sys::ENOSYS) => return copy_by_read(source, dest),
            Err(errno) => return Err(errno),
        }
    }
}

fn copy_by_read(source: i32, dest: i32) -> Result<(), i32> {
    let mut buffer = [0u8; 1024];
    loop {
        let count = sys::read(source, &mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        let mut done = 0usize;
        while done < count {
            let written = sys::write(dest, &buffer[done..count]);
            if written < 0 {
                return Err(-written as i32);
            }
            if written == 0 {
                return Err(EIO);
            }
            done += written as usize;
        }
    }
}

fn word(stat: &[u8; 128], offset: usize) -> u32 {
    u32::from_le_bytes([stat[offset], stat[offset + 1], stat[offset + 2], stat[offset + 3]])
}

fn long(stat: &[u8; 128], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&stat[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}
