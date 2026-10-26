//! ELF64 header and program-header parsing for the guest objects the
//! loader maps: the executable and its `PT_INTERP`. Byte-slice based so
//! the loader can parse from a `pread` buffer without a libc.

pub const EM_AARCH64: u16 = 183;
pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;

pub const PT_LOAD: u32 = 1;
pub const PT_INTERP: u32 = 3;
pub const PT_PHDR: u32 = 6;
pub const PT_GNU_STACK: u32 = 0x6474_e551;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

pub const HEADER_SIZE: usize = 64;
pub const PHDR_SIZE: usize = 56;
/// Distro binaries carry around ten program headers; glibc's `ld.so`
/// fewer. The cap bounds the loader's parse buffers.
pub const MAX_PHDRS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    TooShort,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    NotAArch64,
    BadType,
    BadPhentSize,
    TooManyPhdrs,
    /// `p_vaddr` and `p_offset` disagree modulo the page size, so the
    /// segment cannot be mapped from the file.
    Misaligned,
    /// `p_filesz` exceeds `p_memsz`, or the segment overflows the address
    /// space.
    BadSegment,
    /// No `PT_LOAD` at all.
    NoLoadSegments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub e_type: u16,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_phentsize: u16,
    pub e_phnum: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let slice = bytes.get(offset..offset + 2).ok_or(ElfError::TooShort)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let slice = bytes.get(offset..offset + 4).ok_or(ElfError::TooShort)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    let slice = bytes.get(offset..offset + 8).ok_or(ElfError::TooShort)?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    Ok(u64::from_le_bytes(raw))
}

impl Header {
    /// Parse and validate an ELF header for an AArch64 executable or
    /// shared object. Anything else is `ENOEXEC` material for the caller.
    pub fn parse(bytes: &[u8]) -> Result<Self, ElfError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ElfError::TooShort);
        }
        if bytes[0..4] != [0x7f, b'E', b'L', b'F'] {
            return Err(ElfError::BadMagic);
        }
        if bytes[4] != 2 {
            return Err(ElfError::NotElf64);
        }
        if bytes[5] != 1 {
            return Err(ElfError::NotLittleEndian);
        }
        let e_type = u16_at(bytes, 16)?;
        if u16_at(bytes, 18)? != EM_AARCH64 {
            return Err(ElfError::NotAArch64);
        }
        if e_type != ET_EXEC && e_type != ET_DYN {
            return Err(ElfError::BadType);
        }
        let e_phentsize = u16_at(bytes, 54)?;
        if usize::from(e_phentsize) != PHDR_SIZE {
            return Err(ElfError::BadPhentSize);
        }
        let e_phnum = u16_at(bytes, 56)?;
        if usize::from(e_phnum) > MAX_PHDRS {
            return Err(ElfError::TooManyPhdrs);
        }
        Ok(Self {
            e_type,
            e_entry: u64_at(bytes, 24)?,
            e_phoff: u64_at(bytes, 32)?,
            e_phentsize,
            e_phnum,
        })
    }

    pub fn is_pie(&self) -> bool {
        self.e_type == ET_DYN
    }

    /// Byte size of the program header table.
    pub fn phdr_table_size(&self) -> usize {
        usize::from(self.e_phnum) * PHDR_SIZE
    }
}

impl ProgramHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, ElfError> {
        if bytes.len() < PHDR_SIZE {
            return Err(ElfError::TooShort);
        }
        Ok(Self {
            p_type: u32_at(bytes, 0)?,
            p_flags: u32_at(bytes, 4)?,
            p_offset: u64_at(bytes, 8)?,
            p_vaddr: u64_at(bytes, 16)?,
            p_filesz: u64_at(bytes, 32)?,
            p_memsz: u64_at(bytes, 40)?,
            p_align: u64_at(bytes, 48)?,
        })
    }

    /// Parse a whole table into `out`, returning the entries filled.
    pub fn parse_table<'a>(
        bytes: &[u8],
        count: usize,
        out: &'a mut [ProgramHeader],
    ) -> Result<&'a [ProgramHeader], ElfError> {
        if count > out.len() {
            return Err(ElfError::TooManyPhdrs);
        }
        for (index, slot) in out.iter_mut().enumerate().take(count) {
            let start = index * PHDR_SIZE;
            *slot = Self::parse(bytes.get(start..).ok_or(ElfError::TooShort)?)?;
        }
        Ok(&out[..count])
    }

    /// `mmap` protection bits for this segment.
    pub fn prot(&self) -> u32 {
        let mut prot = 0;
        if self.p_flags & PF_R != 0 {
            prot |= 1;
        }
        if self.p_flags & PF_W != 0 {
            prot |= 2;
        }
        if self.p_flags & PF_X != 0 {
            prot |= 4;
        }
        prot
    }

    /// Check a `PT_LOAD` can be mapped from the file with pages of
    /// `page_size`. Distro arm64 objects are 64 KiB aligned, which
    /// satisfies any smaller page size because the two offsets agree
    /// modulo the alignment.
    pub fn validate_load(&self, page_size: u64) -> Result<(), ElfError> {
        if self.p_filesz > self.p_memsz {
            return Err(ElfError::BadSegment);
        }
        if self.p_vaddr.checked_add(self.p_memsz).is_none() {
            return Err(ElfError::BadSegment);
        }
        if self.p_vaddr % page_size != self.p_offset % page_size {
            return Err(ElfError::Misaligned);
        }
        Ok(())
    }

    pub fn end(&self) -> u64 {
        self.p_vaddr.saturating_add(self.p_memsz)
    }
}

/// Address range `[lo, hi)` the `PT_LOAD` segments of an object cover,
/// page-rounded, relative to the object's link-time base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSpan {
    pub lo: u64,
    pub hi: u64,
}

impl LoadSpan {
    pub fn len(&self) -> u64 {
        self.hi - self.lo
    }

    pub fn is_empty(&self) -> bool {
        self.hi == self.lo
    }
}

pub fn page_floor(value: u64, page_size: u64) -> u64 {
    value & !(page_size - 1)
}

pub fn page_ceil(value: u64, page_size: u64) -> u64 {
    page_floor(value.saturating_add(page_size - 1), page_size)
}

/// Compute the span of all `PT_LOAD` segments, validating each.
pub fn load_span(phdrs: &[ProgramHeader], page_size: u64) -> Result<LoadSpan, ElfError> {
    let mut span: Option<LoadSpan> = None;
    for phdr in phdrs.iter().filter(|phdr| phdr.p_type == PT_LOAD) {
        phdr.validate_load(page_size)?;
        let lo = page_floor(phdr.p_vaddr, page_size);
        let hi = page_ceil(phdr.end(), page_size);
        span = Some(match span {
            None => LoadSpan { lo, hi },
            Some(current) => LoadSpan {
                lo: current.lo.min(lo),
                hi: current.hi.max(hi),
            },
        });
    }
    span.ok_or(ElfError::NoLoadSegments)
}

/// Virtual address of the program header table once the object is
/// mapped at `base`: the kernel reports it through `AT_PHDR` so the
/// dynamic linker can find `PT_DYNAMIC` and `PT_TLS` of the executable.
pub fn phdr_vaddr(header: &Header, phdrs: &[ProgramHeader], base: u64) -> Option<u64> {
    phdrs
        .iter()
        .filter(|phdr| phdr.p_type == PT_LOAD)
        .find(|phdr| {
            header.e_phoff >= phdr.p_offset && header.e_phoff < phdr.p_offset + phdr.p_filesz
        })
        .map(|phdr| base + phdr.p_vaddr + (header.e_phoff - phdr.p_offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(e_type: u16, phnum: u16) -> [u8; HEADER_SIZE] {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[16..18].copy_from_slice(&e_type.to_le_bytes());
        bytes[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
        bytes[24..32].copy_from_slice(&0x1040u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        bytes[56..58].copy_from_slice(&phnum.to_le_bytes());
        bytes
    }

    fn phdr_bytes(p_type: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64) -> [u8; PHDR_SIZE] {
        let mut bytes = [0u8; PHDR_SIZE];
        bytes[0..4].copy_from_slice(&p_type.to_le_bytes());
        bytes[4..8].copy_from_slice(&flags.to_le_bytes());
        bytes[8..16].copy_from_slice(&offset.to_le_bytes());
        bytes[16..24].copy_from_slice(&vaddr.to_le_bytes());
        bytes[32..40].copy_from_slice(&filesz.to_le_bytes());
        bytes[40..48].copy_from_slice(&memsz.to_le_bytes());
        bytes[48..56].copy_from_slice(&0x10000u64.to_le_bytes());
        bytes
    }

    #[test]
    fn parses_a_pie_header() {
        let header = Header::parse(&header_bytes(ET_DYN, 9)).unwrap();
        assert!(header.is_pie());
        assert_eq!(header.e_entry, 0x1040);
        assert_eq!(header.e_phoff, 64);
        assert_eq!(header.phdr_table_size(), 9 * PHDR_SIZE);
    }

    #[test]
    fn rejects_foreign_objects() {
        let mut bytes = header_bytes(ET_DYN, 1);
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        assert_eq!(Header::parse(&bytes), Err(ElfError::NotAArch64));
        let mut bytes = header_bytes(ET_DYN, 1);
        bytes[4] = 1;
        assert_eq!(Header::parse(&bytes), Err(ElfError::NotElf64));
        assert_eq!(Header::parse(&header_bytes(1, 1)), Err(ElfError::BadType));
        assert_eq!(Header::parse(&[0u8; 10]), Err(ElfError::TooShort));
        assert_eq!(Header::parse(&header_bytes(ET_DYN, 65)), Err(ElfError::TooManyPhdrs));
    }

    #[test]
    fn span_and_phdr_address() {
        let mut table = [0u8; 3 * PHDR_SIZE];
        table[0..PHDR_SIZE].copy_from_slice(&phdr_bytes(PT_PHDR, PF_R, 64, 64, 168, 168));
        table[PHDR_SIZE..2 * PHDR_SIZE]
            .copy_from_slice(&phdr_bytes(PT_LOAD, PF_R | PF_X, 0, 0, 0x2_3000, 0x2_3000));
        table[2 * PHDR_SIZE..].copy_from_slice(&phdr_bytes(
            PT_LOAD,
            PF_R | PF_W,
            0x2_3000,
            0x3_3000,
            0x1000,
            0x4000,
        ));
        let mut out = [ProgramHeader::default(); MAX_PHDRS];
        let phdrs = ProgramHeader::parse_table(&table, 3, &mut out).unwrap();
        let span = load_span(phdrs, 4096).unwrap();
        assert_eq!(span, LoadSpan { lo: 0, hi: 0x3_7000 });
        let span16 = load_span(phdrs, 16384).unwrap();
        assert_eq!(span16, LoadSpan { lo: 0, hi: 0x3_8000 });
        let header = Header::parse(&header_bytes(ET_DYN, 3)).unwrap();
        assert_eq!(phdr_vaddr(&header, phdrs, 0x7f00_0000_0000), Some(0x7f00_0000_0040));
        assert_eq!(phdrs[2].prot(), 3);
    }

    #[test]
    fn misaligned_segments_are_rejected() {
        let bytes = phdr_bytes(PT_LOAD, PF_R, 0x1000, 0x1800, 16, 16);
        let phdr = ProgramHeader::parse(&bytes).unwrap();
        assert_eq!(phdr.validate_load(4096), Err(ElfError::Misaligned));
        let bytes = phdr_bytes(PT_LOAD, PF_R, 0, 0, 32, 16);
        let phdr = ProgramHeader::parse(&bytes).unwrap();
        assert_eq!(phdr.validate_load(4096), Err(ElfError::BadSegment));
        assert_eq!(load_span(&[], 4096), Err(ElfError::NoLoadSegments));
    }
}
