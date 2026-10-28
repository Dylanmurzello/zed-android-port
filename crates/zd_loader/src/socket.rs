//! Socket addresses crossing the boundary. A `sun_path` the guest hands
//! in (`bind`, `connect`, `sendto`, `sendmsg`, `sendmmsg`) is translated
//! like any path argument; one the kernel reports (`getsockname`,
//! `getpeername`, `accept4`, `recvfrom`, `recvmsg`, `recvmmsg`) goes
//! through a buffer of ours and reaches the guest in its own spelling.
//! Message headers are copied so the kernel sees our buffers, and what
//! it updates in a header (`msg_namelen`, `msg_controllen`, `msg_flags`,
//! `msg_len`) is copied back to the guest's.

use core::ptr;

use zd_engine_core::path::MAX_PATH;
use zd_engine_core::sockaddr::{UNIX_MAX, unix_path, write_unix};
use zd_engine_core::sysno::nr;
use zd_engine_core::translate::{Translated, Translator};

use crate::handler::{args_of, issue, translate};
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
    match translate_address(translator, bytes, &mut scratch.sockets.rewritten, host, link, target) {
        Ok(Some(new_len)) => {
            args[addr_index] = scratch.sockets.rewritten.as_ptr() as usize;
            args[len_index] = new_len;
        }
        Ok(None) => {}
        Err(errno) => {
            regs[0] = sys::neg_errno(errno);
            return;
        }
    }
    regs[0] = issue(number, &args);
}

/// The address to hand the kernel: `Some(len)` with a rewritten
/// `sockaddr_un` in `out` when the guest's names a filesystem path that
/// translates, `None` when the kernel can take the guest's as it is.
fn translate_address(
    translator: &Translator<'static>,
    bytes: &[u8],
    out: &mut [u8; ADDRESS_BYTES],
    host: &mut [u8; MAX_PATH],
    link: &mut [u8; MAX_PATH],
    target: &mut [u8; MAX_PATH],
) -> Result<Option<usize>, i32> {
    let Some(path) = unix_path(bytes) else {
        return Ok(None);
    };
    let host_len = match translate(translator, path, host, link, target)? {
        Translated::Host(len) => len,
        _ => return Ok(None),
    };
    write_unix(&host[..host_len], out).map(Some).ok_or(sys::ENAMETOOLONG)
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
    deliver(
        translator,
        &sockets.addresses[0][..reported.min(ADDRESS_BYTES)],
        reported,
        address,
        capacity,
        length,
        &mut sockets.rewritten,
        &mut scratch.paths[0],
    );
}

/// Copy a kernel-reported address to the guest in its own spelling and
/// write the full length the kernel would report.
fn deliver(
    translator: &Translator<'static>,
    reported: &[u8],
    reported_len: usize,
    address: *mut u8,
    capacity: usize,
    length: *mut u32,
    rewritten: &mut [u8; ADDRESS_BYTES],
    path: &mut [u8; MAX_PATH],
) {
    let (bytes, len) = detranslate(translator, reported, reported_len, rewritten, path);
    let copy = len.min(capacity).min(bytes.len());
    // SAFETY: the guest's buffer of `capacity` bytes and its length word,
    // as handed to the kernel.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), address, copy);
        length.write_unaligned(len as u32);
    }
}

/// The guest's spelling of a kernel-reported address: a `sun_path` under
/// the rootfs or a binding is mapped back; anything else is returned as
/// it came, with the kernel's length (which exceeds the bytes when the
/// kernel truncated).
fn detranslate<'a>(
    translator: &Translator<'static>,
    reported: &'a [u8],
    reported_len: usize,
    rewritten: &'a mut [u8; ADDRESS_BYTES],
    path: &mut [u8; MAX_PATH],
) -> (&'a [u8], usize) {
    let Some(host) = unix_path(reported) else {
        return (reported, reported_len);
    };
    if host.first() != Some(&b'/') {
        return (reported, reported_len);
    }
    let Ok(guest_len) = translator.host_to_guest(host, path) else {
        return (reported, reported_len);
    };
    match write_unix(&path[..guest_len], rewritten) {
        Some(len) => (&rewritten[..len], len),
        None => (reported, reported_len),
    }
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
        match translate_address(translator, bytes, &mut sockets.addresses[0], host, link, target) {
            Ok(Some(len)) => {
                copy.name = sockets.addresses[0].as_ptr() as u64;
                copy.namelen = len as u32;
            }
            Ok(None) => {
                regs[0] = issue(number, &args);
                return;
            }
            Err(errno) => {
                regs[0] = sys::neg_errno(errno);
                return;
            }
        }
        sockets.headers[0].header = copy;
        args[1] = &sockets.headers[0].header as *const MsgHdr as usize;
        regs[0] = issue(number, &args);
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
    );
}

/// After a receive: the address to the guest's buffer, and the header
/// fields the kernel updated to the guest's header.
fn finish_receive(
    translator: &Translator<'static>,
    updated: &MsgHdr,
    reported_address: &[u8; ADDRESS_BYTES],
    header: *mut MsgHdr,
    guest_name: *mut u8,
    capacity: usize,
    rewritten: &mut [u8; ADDRESS_BYTES],
    path: &mut [u8; MAX_PATH],
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
            path,
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
                    Ok(Some(len)) => {
                        copy.name = sockets.addresses[index].as_ptr() as u64;
                        copy.namelen = len as u32;
                        rewritten_any = true;
                    }
                    Ok(None) => {}
                    Err(errno) => {
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
            );
        }
    }
}
