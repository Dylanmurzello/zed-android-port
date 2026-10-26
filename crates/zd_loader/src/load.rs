//! Map a guest executable and its `PT_INTERP` the way the kernel's ELF
//! loader would, from files reached through the path translator.

use core::fmt;

use zd_engine_core::elf::{
    ElfError, HEADER_SIZE, Header, MAX_PHDRS, PHDR_SIZE, PT_INTERP, PT_LOAD, ProgramHeader,
    load_span, page_ceil, page_floor, phdr_vaddr,
};
use zd_engine_core::path::MAX_PATH;
use zd_engine_core::translate::{Translated, Translator};

use crate::config;
use crate::sys;

pub const MAX_INTERP: usize = 256;

pub struct Loaded {
    pub entry: u64,
    pub base: u64,
    pub phdr: u64,
    pub phnum: u64,
    pub interp: [u8; MAX_INTERP],
    pub interp_len: usize,
    pub interp_base: u64,
    pub interp_entry: u64,
}

impl Loaded {
    pub fn interp_path(&self) -> &[u8] {
        &self.interp[..self.interp_len]
    }

    /// Where the guest starts: the dynamic linker when there is one.
    pub fn start_address(&self) -> u64 {
        if self.interp_len > 0 {
            self.interp_entry
        } else {
            self.entry
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LoadError {
    Translate,
    Open(i32),
    Read(i32),
    ShortRead,
    Elf(ElfError),
    Reserve(i32),
    Map(i32),
    Protect(i32),
    InterpTooLong,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Translate => write!(f, "path cannot be translated"),
            Self::Open(errno) => write!(f, "open failed: errno {errno}"),
            Self::Read(errno) => write!(f, "read failed: errno {errno}"),
            Self::ShortRead => write!(f, "file truncated"),
            Self::Elf(error) => write!(f, "not a loadable AArch64 ELF ({error:?})"),
            Self::Reserve(errno) => write!(f, "reserving the address range failed: errno {errno}"),
            Self::Map(errno) => write!(f, "mapping a segment failed: errno {errno}"),
            Self::Protect(errno) => write!(f, "mprotect failed: errno {errno}"),
            Self::InterpTooLong => write!(f, "PT_INTERP path is empty or too long"),
        }
    }
}

struct Mapped {
    base: u64,
    entry: u64,
    phdr: u64,
    phnum: u64,
    interp: [u8; MAX_INTERP],
    interp_len: usize,
}

pub fn load(guest_path: &[u8]) -> Result<Loaded, LoadError> {
    let translator = config::translator().ok_or(LoadError::Translate)?;
    let page = config::get().page_size as u64;
    let mut host = [0u8; MAX_PATH];

    let fd = open_guest(translator, guest_path, &mut host)?;
    let exe = map_object(fd, page);
    close_quietly(fd);
    let exe = exe?;

    let mut loaded = Loaded {
        entry: exe.entry,
        base: exe.base,
        phdr: exe.phdr,
        phnum: exe.phnum,
        interp: exe.interp,
        interp_len: exe.interp_len,
        interp_base: 0,
        interp_entry: 0,
    };
    if exe.interp_len > 0 {
        let fd = open_guest(translator, &exe.interp[..exe.interp_len], &mut host)?;
        let interp = map_object(fd, page);
        close_quietly(fd);
        let interp = interp?;
        loaded.interp_base = interp.base;
        loaded.interp_entry = interp.entry;
    }
    Ok(loaded)
}

/// Open a guest path for reading. Needs a NUL-terminated copy either
/// way, so the host buffer doubles as that copy for relative paths.
pub fn open_guest(
    translator: &Translator<'static>,
    guest: &[u8],
    host: &mut [u8; MAX_PATH],
) -> Result<i32, LoadError> {
    match translator.guest_to_host(guest, host) {
        Ok(Translated::Host(_)) => {}
        Ok(Translated::Relative) => {
            if guest.len() >= MAX_PATH {
                return Err(LoadError::Translate);
            }
            host[..guest.len()].copy_from_slice(guest);
            host[guest.len()] = 0;
        }
        Ok(Translated::ProcExe(_)) | Ok(Translated::ProcAuxv(_)) | Err(_) => {
            return Err(LoadError::Translate);
        }
    }
    sys::openat(
        sys::AT_FDCWD,
        host.as_ptr(),
        sys::O_RDONLY | sys::O_CLOEXEC,
        0,
    )
    .map_err(LoadError::Open)
}

fn close_quietly(fd: i32) {
    if let Err(_errno) = sys::close(fd) {
        // The mapping already holds its own reference to the file.
    }
}

fn read_exact(fd: i32, buffer: &mut [u8], offset: u64) -> Result<(), LoadError> {
    let mut done = 0usize;
    while done < buffer.len() {
        let count = sys::pread64(fd, &mut buffer[done..], offset + done as u64)
            .map_err(LoadError::Read)?;
        if count == 0 {
            return Err(LoadError::ShortRead);
        }
        done += count;
    }
    Ok(())
}

fn map_object(fd: i32, page: u64) -> Result<Mapped, LoadError> {
    let mut header_bytes = [0u8; HEADER_SIZE];
    read_exact(fd, &mut header_bytes, 0)?;
    let header = Header::parse(&header_bytes).map_err(LoadError::Elf)?;

    let mut table = [0u8; MAX_PHDRS * PHDR_SIZE];
    let table_len = header.phdr_table_size();
    read_exact(fd, &mut table[..table_len], header.e_phoff)?;
    let mut phdrs = [ProgramHeader::default(); MAX_PHDRS];
    let phdrs =
        ProgramHeader::parse_table(&table[..table_len], usize::from(header.e_phnum), &mut phdrs)
            .map_err(LoadError::Elf)?;

    let span = load_span(phdrs, page).map_err(LoadError::Elf)?;
    let base = if header.is_pie() {
        // Reserve the whole span so the segments land at their relative
        // offsets; each PT_LOAD then replaces its part of the reservation.
        let reserved = sys::mmap(
            0,
            span.len() as usize,
            sys::PROT_NONE,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
            -1,
            0,
        )
        .map_err(LoadError::Reserve)?;
        (reserved as u64).wrapping_sub(span.lo)
    } else {
        0
    };

    for phdr in phdrs.iter().filter(|phdr| phdr.p_type == PT_LOAD) {
        map_segment(fd, phdr, base, page)?;
    }

    let mut mapped = Mapped {
        base,
        entry: base.wrapping_add(header.e_entry),
        phdr: phdr_vaddr(&header, phdrs, base).unwrap_or(base.wrapping_add(header.e_phoff)),
        phnum: u64::from(header.e_phnum),
        interp: [0; MAX_INTERP],
        interp_len: 0,
    };
    if let Some(interp) = phdrs.iter().find(|phdr| phdr.p_type == PT_INTERP) {
        let len = interp.p_filesz as usize;
        if len == 0 || len > MAX_INTERP {
            return Err(LoadError::InterpTooLong);
        }
        read_exact(fd, &mut mapped.interp[..len], interp.p_offset)?;
        mapped.interp_len = mapped.interp[..len]
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(len);
        if mapped.interp_len == 0 {
            return Err(LoadError::InterpTooLong);
        }
    }
    Ok(mapped)
}

fn map_segment(fd: i32, phdr: &ProgramHeader, base: u64, page: u64) -> Result<(), LoadError> {
    let prot = phdr.prot() as usize;
    let segment_start = base.wrapping_add(phdr.p_vaddr);
    let map_start = page_floor(segment_start, page);
    let file_end = segment_start.wrapping_add(phdr.p_filesz);
    let memory_end = segment_start.wrapping_add(phdr.p_memsz);

    if phdr.p_filesz > 0 {
        let len = page_ceil(file_end, page) - map_start;
        sys::mmap(
            map_start as usize,
            len as usize,
            prot,
            sys::MAP_PRIVATE | sys::MAP_FIXED,
            fd,
            page_floor(phdr.p_offset, page),
        )
        .map_err(LoadError::Map)?;
    }

    if phdr.p_memsz > phdr.p_filesz {
        let file_page_end = if phdr.p_filesz > 0 {
            page_ceil(file_end, page)
        } else {
            map_start
        };
        if phdr.p_filesz > 0 && file_page_end > file_end {
            // The last file-backed page carries whatever follows the
            // segment in the file; bss must read as zeros.
            let tail = (file_page_end - file_end) as usize;
            let page_start = page_floor(file_end, page) as usize;
            let read_only = prot & sys::PROT_WRITE == 0;
            if read_only {
                sys::mprotect(page_start, page as usize, prot | sys::PROT_WRITE)
                    .map_err(LoadError::Protect)?;
            }
            // SAFETY: the page was just mapped writable for this process.
            unsafe { core::ptr::write_bytes(file_end as *mut u8, 0, tail) };
            if read_only {
                sys::mprotect(page_start, page as usize, prot).map_err(LoadError::Protect)?;
            }
        }
        let anonymous_end = page_ceil(memory_end, page);
        if anonymous_end > file_page_end {
            sys::mmap(
                file_page_end as usize,
                (anonymous_end - file_page_end) as usize,
                prot,
                sys::MAP_PRIVATE | sys::MAP_FIXED | sys::MAP_ANONYMOUS,
                -1,
                0,
            )
            .map_err(LoadError::Map)?;
        }
    }
    Ok(())
}
