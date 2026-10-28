//! Socket addresses crossing the boundary. A `sun_path` the guest hands
//! in (`bind`, `connect`, `sendto`, `sendmsg`, `sendmmsg`) is translated
//! like any path argument; one the kernel reports (`getsockname`,
//! `getpeername`, `accept4`, `recvfrom`, `recvmsg`, `recvmmsg`) goes
//! through a buffer of ours and reaches the guest in its own spelling.
//! Message headers are copied so the kernel sees our buffers, and what
//! it updates in a header (`msg_namelen`, `msg_controllen`, `msg_flags`,
//! `msg_len`) is copied back to the guest's.
//!
//! `sun_path` holds 107 bytes and the host spelling is 33 to 36 bytes
//! longer than the guest's, so a guest path legal on Linux can be too
//! long here. Such a path reaches the kernel as `/proc/<pid>/fd/<n>/<entry>`
//! with the directory open on descriptor `n`, which `openat` resolves at
//! any length. A bind keeps that descriptor with the socket: the kernel
//! reports a socket's name as it was bound, so the report is mapped back
//! by reading the descriptor's link, from this process or a peer's.

use core::ptr;

use zd_engine_core::path::{MAX_PATH, Writer, is_absolute};
use zd_engine_core::sockaddr::{SUN_PATH_MAX, UNIX_MAX, proc_fd_link, unix_path, write_unix};
use zd_engine_core::sysno::nr;
use zd_engine_core::translate::{Translated, Translator};

use crate::handler::{args_of, issue, translate};
use crate::load::close_quietly;
use crate::log;
use crate::scratch::Scratch;
use crate::sys;

/// Messages handled per `sendmmsg` or `recvmmsg` call. The kernel's
/// count applies to that many, and a guest asking for more sees the
/// shorter count the way it would after a partial send or receive.
pub const MESSAGES: usize = 16;
/// Room for any address family the kernel reports.
const ADDRESS_BYTES: usize = 128;

/// Kernel `struct user_msghdr`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MsgHdr {
    pub name: u64,
    pub namelen: u32,
    padding: u32,
    pub iov: u64,
    pub iovlen: u64,
    pub control: u64,
    pub controllen: u64,
    pub flags: i32,
    padding_end: i32,
}

/// Kernel `struct mmsghdr`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MMsgHdr {
    pub header: MsgHdr,
    pub len: u32,
    padding: u32,
}

const EMPTY_MESSAGE: MMsgHdr = MMsgHdr {
    header: MsgHdr {
        name: 0,
        namelen: 0,
        padding: 0,
        iov: 0,
        iovlen: 0,
        control: 0,
        controllen: 0,
        flags: 0,
        padding_end: 0,
    },
    len: 0,
    padding: 0,
};

#[repr(C, align(8))]
pub struct SocketScratch {
    headers: [MMsgHdr; MESSAGES],
    addresses: [[u8; ADDRESS_BYTES]; MESSAGES],
    rewritten: [u8; ADDRESS_BYTES],
}

impl SocketScratch {
    pub const fn new() -> Self {
        Self {
            headers: [EMPTY_MESSAGE; MESSAGES],
            addresses: [[0; ADDRESS_BYTES]; MESSAGES],
            rewritten: [0; ADDRESS_BYTES],
        }
    }
}

fn is_error(result: u64) -> bool {
    (result as i64) < 0
}

/// A rewritten address for the kernel: its length, and the directory
/// descriptor a long path went through.
struct Rewritten {
    len: usize,
    directory: Option<i32>,
}

/// `bind`, `connect`, `sendto`: the guest's address at `addr_index`
/// with its length at `len_index`.
#[inline(never)]
pub fn address_in(
    translator: &Translator<'static>,
    number: u32,
    regs: &mut [u64; 31],
    addr_index: usize,
    len_index: usize,
    scratch: &mut Scratch,
) {
    let mut args = args_of(regs);
    let address = regs[addr_index] as *const u8;
    let len = regs[len_index] as usize;
    if address.is_null() || len < 3 {
        regs[0] = issue(number, &args);
        return;
    }
    // SAFETY: a guest sockaddr of `len` bytes, as handed to the kernel.
    let bytes = unsafe { core::slice::from_raw_parts(address, len.min(UNIX_MAX)) };
    let [host, link, target, ..] = &mut scratch.paths;
    let mut directory = None;
    match translate_address(translator, bytes, &mut scratch.sockets.rewritten, host, link, target) {
        Ok(Some(rewritten)) => {
            args[addr_index] = scratch.sockets.rewritten.as_ptr() as usize;
            args[len_index] = rewritten.len;
            directory = rewritten.directory;
        }
        Ok(None) => {}
        Err(errno) => {
            regs[0] = sys::neg_errno(errno);
            return;
        }
    }
    regs[0] = issue(number, &args);
    // A bound socket keeps its directory open (see the module doc); any
    // other use of the descriptor, and a failed bind, is over.
    if let Some(fd) = directory
        && (number != nr::BIND || is_error(regs[0]))
    {
        close_quietly(fd);
    }
}

/// The address to hand the kernel: a rewritten `sockaddr_un` in `out`
/// when the guest's names a filesystem path that translates, `None` when
/// the kernel can take the guest's as it is.
fn translate_address(
    translator: &Translator<'static>,
    bytes: &[u8],
    out: &mut [u8; ADDRESS_BYTES],
    host: &mut [u8; MAX_PATH],
    link: &mut [u8; MAX_PATH],
    target: &mut [u8; MAX_PATH],
) -> Result<Option<Rewritten>, i32> {
    let Some(path) = unix_path(bytes) else {
        return Ok(None);
    };
    let host_len = match translate(translator, path, host, link, target)? {
        Translated::Host(len) => len,
        _ => return Ok(None),
    };
    if host_len < SUN_PATH_MAX {
        let len = write_unix(&host[..host_len], out).ok_or(sys::ENAMETOOLONG)?;
        return Ok(Some(Rewritten { len, directory: None }));
    }
    through_directory(&host[..host_len], out, link, target).map(Some)
}

/// A host spelling too long for `sun_path`, given to the kernel as
/// `/proc/<pid>/fd/<n>/<entry>` with the directory open on `n`.
fn through_directory(
    host: &[u8],
    out: &mut [u8; ADDRESS_BYTES],
    directory: &mut [u8; MAX_PATH],
    spelling: &mut [u8; MAX_PATH],
) -> Result<Rewritten, i32> {
    let split = host.iter().rposition(|&byte| byte == b'/').ok_or(sys::ENAMETOOLONG)?;
    let (parent, entry) = (&host[..split.max(1)], &host[split + 1..]);
    if entry.is_empty() || parent.len() >= MAX_PATH {
        return Err(sys::ENAMETOOLONG);
    }
    directory[..parent.len()].copy_from_slice(parent);
    directory[parent.len()] = 0;
    let fd = sys::openat(
        sys::AT_FDCWD,
        directory.as_ptr(),
        sys::O_RDONLY | sys::O_DIRECTORY | sys::O_CLOEXEC,
        0,
    )?;
    let mut digits = [0u8; 20];
    let mut writer = Writer::new(&mut spelling[..]);
    let written = writer
        .push(b"/proc/")
        .and_then(|()| writer.push(log::itoa(&mut digits, u64::from(sys::getpid()))))
        .and_then(|()| writer.push(b"/fd/"))
        .and_then(|()| writer.push(log::itoa(&mut digits, fd as u64)))
        .and_then(|()| writer.push(b"/"))
        .and_then(|()| writer.push(entry));
    if written.is_err() {
        close_quietly(fd);
        return Err(sys::ENAMETOOLONG);
    }
    let len = writer.finish();
    match write_unix(&spelling[..len], out) {
        Some(len) => Ok(Rewritten {
            len,
            directory: Some(fd),
        }),
        None => {
            close_quietly(fd);
            Err(sys::ENAMETOOLONG)
        }
    }
}

/// `getsockname`, `getpeername`, `accept4`, `recvfrom`: the kernel fills
/// a buffer of ours, and the guest gets the address in its own spelling
/// under the kernel's own rule (the length word receives the full
/// length, the buffer what fits).
#[inline(never)]
pub fn address_out(
    translator: &Translator<'static>,
    number: u32,
    regs: &mut [u64; 31],
    addr_index: usize,
    len_index: usize,
    scratch: &mut Scratch,
) {
    let mut args = args_of(regs);
    let address = regs[addr_index] as *mut u8;
    let length = regs[len_index] as *mut u32;
    if address.is_null() || length.is_null() {
        regs[0] = issue(number, &args);
        return;
    }
    // SAFETY: the guest's length word, as handed to the kernel.
    let capacity = unsafe { length.read_unaligned() } as usize;
    let sockets = &mut scratch.sockets;
    let mut reported_len = ADDRESS_BYTES as u32;
    args[addr_index] = sockets.addresses[0].as_mut_ptr() as usize;
    args[len_index] = &mut reported_len as *mut u32 as usize;
    let result = issue(number, &args);
    regs[0] = result;
    if is_error(result) {
        return;
    }
    let reported = reported_len as usize;
    let [path_a, path_b, ..] = &mut scratch.paths;
    deliver(
        translator,
        &sockets.addresses[0][..reported.min(ADDRESS_BYTES)],
        reported,
        address,
        capacity,
        length,
        &mut sockets.rewritten,
        path_a,
        path_b,
    );
}

/// Copy a kernel-reported address to the guest in its own spelling and
/// write the full length the kernel would report.
#[allow(clippy::too_many_arguments)]
fn deliver(
    translator: &Translator<'static>,
    reported: &[u8],
    reported_len: usize,
    address: *mut u8,
    capacity: usize,
    length: *mut u32,
    rewritten: &mut [u8; ADDRESS_BYTES],
    path_a: &mut [u8; MAX_PATH],
    path_b: &mut [u8; MAX_PATH],
) {
    let (bytes, len) = detranslate(translator, reported, reported_len, rewritten, path_a, path_b);
    let copy = len.min(capacity).min(bytes.len());
    // SAFETY: the guest's buffer of `capacity` bytes and its length word,
    // as handed to the kernel.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), address, copy);
        length.write_unaligned(len as u32);
    }
}

/// The guest's spelling of a kernel-reported address: a `sun_path` under
/// the rootfs or a binding is mapped back, a descriptor spelling is read
/// back first; anything else is returned as it came, with the kernel's
/// length (which exceeds the bytes when the kernel truncated).
fn detranslate<'a>(
    translator: &Translator<'static>,
    reported: &'a [u8],
    reported_len: usize,
    rewritten: &'a mut [u8; ADDRESS_BYTES],
    path_a: &mut [u8; MAX_PATH],
    path_b: &mut [u8; MAX_PATH],
) -> (&'a [u8], usize) {
    let Some(host) = unix_path(reported) else {
        return (reported, reported_len);
    };
    if host.first() != Some(&b'/') {
        return (reported, reported_len);
    }
    let guest_len = match proc_fd_link(host) {
        Some((link, entry)) => match through_link(translator, link, entry, path_a, path_b) {
            Some(len) => len,
            None => return (reported, reported_len),
        },
        None => match translator.host_to_guest(host, path_a) {
            Ok(len) => len,
            Err(_) => return (reported, reported_len),
        },
    };
    match write_unix(&path_a[..guest_len], rewritten) {
        Some(len) => (&rewritten[..len], len),
        None => (reported, reported_len),
    }
}

/// The guest path behind `/proc/<pid>/fd/<n>/<entry>`: the directory the
/// descriptor holds, mapped back, then the entry. Into `out`.
fn through_link(
    translator: &Translator<'static>,
    link: &[u8],
    entry: &[u8],
    out: &mut [u8; MAX_PATH],
    scratch: &mut [u8; MAX_PATH],
) -> Option<usize> {
    if link.len() >= MAX_PATH {
        return None;
    }
    scratch[..link.len()].copy_from_slice(link);
    scratch[link.len()] = 0;
    let directory_len = sys::readlinkat(sys::AT_FDCWD, scratch.as_ptr(), &mut out[..MAX_PATH - 1]).ok()?;
    if !is_absolute(&out[..directory_len]) {
        return None;
    }
    let guest_directory_len = translator.host_to_guest(&out[..directory_len], scratch).ok()?;
    let mut writer = Writer::resume(&mut scratch[..], guest_directory_len);
    writer.push_component(entry).ok()?;
    let len = writer.finish();
    out[..len].copy_from_slice(&scratch[..len]);
    Some(len)
}

/// `sendmsg` and `recvmsg`: the name in the header, when there is one.
#[inline(never)]
pub fn message(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31], scratch: &mut Scratch) {
    let mut args = args_of(regs);
    let header = regs[1] as *mut MsgHdr;
    if header.is_null() {
        regs[0] = issue(number, &args);
        return;
    }
    // SAFETY: the guest's header, as handed to the kernel.
    let mut copy = unsafe { header.read_unaligned() };
    if copy.name == 0 || copy.namelen == 0 {
        regs[0] = issue(number, &args);
        return;
    }
    let guest_name = copy.name as *mut u8;
    let capacity = copy.namelen as usize;
    let sockets = &mut scratch.sockets;
    let [host, link, target, ..] = &mut scratch.paths;
    if number == nr::SENDMSG {
        // SAFETY: the guest's address of `capacity` bytes, as handed to the kernel.
        let bytes = unsafe { core::slice::from_raw_parts(guest_name, capacity.min(UNIX_MAX)) };
        let directory = match translate_address(translator, bytes, &mut sockets.addresses[0], host, link, target) {
            Ok(Some(rewritten)) => {
                copy.name = sockets.addresses[0].as_ptr() as u64;
                copy.namelen = rewritten.len as u32;
                rewritten.directory
            }
            Ok(None) => {
                regs[0] = issue(number, &args);
                return;
            }
            Err(errno) => {
                regs[0] = sys::neg_errno(errno);
                return;
            }
        };
        sockets.headers[0].header = copy;
        args[1] = &sockets.headers[0].header as *const MsgHdr as usize;
        regs[0] = issue(number, &args);
        if let Some(fd) = directory {
            close_quietly(fd);
        }
        return;
    }
    copy.name = sockets.addresses[0].as_mut_ptr() as u64;
    copy.namelen = ADDRESS_BYTES as u32;
    sockets.headers[0].header = copy;
    args[1] = &mut sockets.headers[0].header as *mut MsgHdr as usize;
    let result = issue(number, &args);
    regs[0] = result;
    if is_error(result) {
        return;
    }
    let updated = sockets.headers[0].header;
    finish_receive(
        translator,
        &updated,
        &sockets.addresses[0],
        header,
        guest_name,
        capacity,
        &mut sockets.rewritten,
        host,
        link,
    );
}

/// After a receive: the address to the guest's buffer, and the header
/// fields the kernel updated to the guest's header.
#[allow(clippy::too_many_arguments)]
fn finish_receive(
    translator: &Translator<'static>,
    updated: &MsgHdr,
    reported_address: &[u8; ADDRESS_BYTES],
    header: *mut MsgHdr,
    guest_name: *mut u8,
    capacity: usize,
    rewritten: &mut [u8; ADDRESS_BYTES],
    path_a: &mut [u8; MAX_PATH],
    path_b: &mut [u8; MAX_PATH],
) {
    let reported = updated.namelen as usize;
    // SAFETY: the guest's header, as handed to the kernel; its fields are
    // written the way the kernel writes them.
    unsafe {
        deliver(
            translator,
            &reported_address[..reported.min(ADDRESS_BYTES)],
            reported,
            guest_name,
            capacity,
            ptr::addr_of_mut!((*header).namelen),
            rewritten,
            path_a,
            path_b,
        );
        ptr::addr_of_mut!((*header).controllen).write_unaligned(updated.controllen);
        ptr::addr_of_mut!((*header).flags).write_unaligned(updated.flags);
    }
}

/// `sendmmsg` and `recvmmsg`: up to `MESSAGES` entries, each handled as
/// `message` handles one.
#[inline(never)]
pub fn multi_message(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31], scratch: &mut Scratch) {
    let mut args = args_of(regs);
    let vector = regs[1] as *mut MMsgHdr;
    let count = (regs[2] as usize).min(MESSAGES);
    if vector.is_null() || count == 0 {
        regs[0] = issue(number, &args);
        return;
    }
    let sockets = &mut scratch.sockets;
    let [host, link, target, ..] = &mut scratch.paths;
    let mut guest_names = [(ptr::null_mut::<u8>(), 0usize); MESSAGES];
    let mut directories = [None::<i32>; MESSAGES];
    let mut rewritten_any = false;
    for index in 0..count {
        // SAFETY: `count` guest entries, as handed to the kernel.
        let entry = unsafe { vector.add(index).read_unaligned() };
        let mut copy = entry.header;
        guest_names[index] = (copy.name as *mut u8, copy.namelen as usize);
        if copy.name != 0 && copy.namelen != 0 {
            if number == nr::SENDMMSG {
                // SAFETY: the guest's address, as handed to the kernel.
                let bytes = unsafe {
                    core::slice::from_raw_parts(copy.name as *const u8, (copy.namelen as usize).min(UNIX_MAX))
                };
                match translate_address(translator, bytes, &mut sockets.addresses[index], host, link, target) {
                    Ok(Some(rewritten)) => {
                        copy.name = sockets.addresses[index].as_ptr() as u64;
                        copy.namelen = rewritten.len as u32;
                        directories[index] = rewritten.directory;
                        rewritten_any = true;
                    }
                    Ok(None) => {}
                    Err(errno) => {
                        close_all(&directories);
                        regs[0] = sys::neg_errno(errno);
                        return;
                    }
                }
            } else {
                copy.name = sockets.addresses[index].as_mut_ptr() as u64;
                copy.namelen = ADDRESS_BYTES as u32;
                rewritten_any = true;
            }
        }
        sockets.headers[index] = MMsgHdr {
            header: copy,
            len: 0,
            padding: 0,
        };
    }
    if !rewritten_any {
        regs[0] = issue(number, &args);
        return;
    }
    args[1] = sockets.headers.as_mut_ptr() as usize;
    args[2] = count;
    let result = issue(number, &args);
    close_all(&directories);
    regs[0] = result;
    if is_error(result) {
        return;
    }
    let done = (result as usize).min(count);
    for index in 0..done {
        let updated = sockets.headers[index];
        // SAFETY: the guest's entry, as handed to the kernel; written the
        // way the kernel writes it.
        let entry = unsafe { vector.add(index) };
        unsafe { ptr::addr_of_mut!((*entry).len).write_unaligned(updated.len) };
        let (guest_name, capacity) = guest_names[index];
        if number == nr::RECVMMSG && !guest_name.is_null() && capacity != 0 {
            finish_receive(
                translator,
                &updated.header,
                &sockets.addresses[index],
                unsafe { ptr::addr_of_mut!((*entry).header) },
                guest_name,
                capacity,
                &mut sockets.rewritten,
                host,
                link,
            );
        }
    }
}

fn close_all(directories: &[Option<i32>; MESSAGES]) {
    for fd in directories.iter().flatten() {
        close_quietly(*fd);
    }
}
