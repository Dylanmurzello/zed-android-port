//! The initial process stack the kernel builds for an executable:
//! `[argc][argv..][0][envp..][0][auxv pairs..][AT_NULL 0]`. The loader
//! receives it for itself and rebuilds it for the guest below the
//! original; the string areas above stay where the kernel put them.
//! Everything here works on word slices so the rebuild is testable
//! without a real stack.

pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_PLATFORM: u64 = 15;
pub const AT_HWCAP: u64 = 16;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_HWCAP2: u64 = 26;
pub const AT_EXECFN: u64 = 31;
pub const AT_SYSINFO_EHDR: u64 = 33;
pub const AT_MINSIGSTKSZ: u64 = 51;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackError {
    /// The output buffer cannot hold the rebuilt vector.
    TooSmall,
}

/// One `(a_type, a_val)` pair.
pub type AuxEntry = [u64; 2];

/// Incremental writer for `[argc][argv][0][envp][0][auxv][0 0]`. The
/// loader points it at the gap below the original vector and feeds the
/// guest's argv, env and patched auxv in order; nothing is buffered.
pub struct VectorWriter<'a> {
    out: &'a mut [u64],
    index: usize,
    argc: usize,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Argv,
    Envp,
    Auxv,
    Done,
}

impl<'a> VectorWriter<'a> {
    /// `argc` is written up front so the vector is valid even if the
    /// caller counts entries while pushing them.
    pub fn new(out: &'a mut [u64], argc: usize) -> Result<Self, StackError> {
        if out.is_empty() {
            return Err(StackError::TooSmall);
        }
        out[0] = argc as u64;
        Ok(Self {
            out,
            index: 1,
            argc,
            phase: Phase::Argv,
        })
    }

    /// Words a vector with these counts occupies.
    pub fn words_needed(argc: usize, envc: usize, auxc: usize) -> usize {
        1 + argc + 1 + envc + 1 + auxc * 2 + 2
    }

    fn put(&mut self, word: u64) -> Result<(), StackError> {
        let slot = self.out.get_mut(self.index).ok_or(StackError::TooSmall)?;
        *slot = word;
        self.index += 1;
        Ok(())
    }

    pub fn push_arg(&mut self, pointer: u64) -> Result<(), StackError> {
        debug_assert_eq!(self.phase, Phase::Argv);
        self.put(pointer)
    }

    pub fn end_args(&mut self) -> Result<(), StackError> {
        debug_assert_eq!(self.phase, Phase::Argv);
        debug_assert_eq!(self.index, 1 + self.argc, "argc disagrees with pushed args");
        self.phase = Phase::Envp;
        self.put(0)
    }

    pub fn push_env(&mut self, pointer: u64) -> Result<(), StackError> {
        debug_assert_eq!(self.phase, Phase::Envp);
        self.put(pointer)
    }

    pub fn end_env(&mut self) -> Result<(), StackError> {
        debug_assert_eq!(self.phase, Phase::Envp);
        self.phase = Phase::Auxv;
        self.put(0)
    }

    pub fn push_aux(&mut self, entry: AuxEntry) -> Result<(), StackError> {
        debug_assert_eq!(self.phase, Phase::Auxv);
        self.put(entry[0])?;
        self.put(entry[1])
    }

    /// Terminate with `AT_NULL` and return the words written.
    pub fn finish(mut self) -> Result<usize, StackError> {
        debug_assert_eq!(self.phase, Phase::Auxv);
        self.put(AT_NULL)?;
        self.put(0)?;
        self.phase = Phase::Done;
        Ok(self.index)
    }
}

/// Set `key` to `value` in place, or report that it is absent.
pub fn auxv_set(auxv: &mut [AuxEntry], key: u64, value: u64) -> bool {
    match auxv.iter_mut().find(|entry| entry[0] == key) {
        Some(entry) => {
            entry[1] = value;
            true
        }
        None => false,
    }
}

pub fn auxv_get(auxv: &[AuxEntry], key: u64) -> Option<u64> {
    auxv.iter().find(|entry| entry[0] == key).map(|entry| entry[1])
}

/// `KEY` part of a `KEY=VALUE` environment entry.
pub fn env_key(entry: &[u8]) -> &[u8] {
    match entry.iter().position(|&byte| byte == b'=') {
        Some(index) => &entry[..index],
        None => entry,
    }
}

/// `VALUE` part of a `KEY=VALUE` entry when its key is `key`.
pub fn env_value<'a>(entry: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let rest = entry.strip_prefix(key)?;
    rest.strip_prefix(b"=")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_layout() {
        let mut out = [0xAAu64; 32];
        let mut writer = VectorWriter::new(&mut out, 2).unwrap();
        writer.push_arg(0x10).unwrap();
        writer.push_arg(0x20).unwrap();
        writer.end_args().unwrap();
        writer.push_env(0x30).unwrap();
        writer.end_env().unwrap();
        writer.push_aux([AT_PAGESZ, 4096]).unwrap();
        writer.push_aux([AT_RANDOM, 0x7000]).unwrap();
        let words = writer.finish().unwrap();
        assert_eq!(words, VectorWriter::words_needed(2, 1, 2));
        assert_eq!(
            &out[..words],
            &[2, 0x10, 0x20, 0, 0x30, 0, AT_PAGESZ, 4096, AT_RANDOM, 0x7000, 0, 0]
        );
    }

    #[test]
    fn vector_needs_room() {
        let mut out = [0u64; 5];
        let mut writer = VectorWriter::new(&mut out, 1).unwrap();
        writer.push_arg(1).unwrap();
        writer.end_args().unwrap();
        writer.end_env().unwrap();
        assert_eq!(writer.finish(), Err(StackError::TooSmall));
        let mut out = [0u64; 6];
        let mut writer = VectorWriter::new(&mut out, 1).unwrap();
        writer.push_arg(1).unwrap();
        writer.end_args().unwrap();
        writer.end_env().unwrap();
        assert_eq!(writer.finish(), Ok(6));
        assert_eq!(VectorWriter::words_needed(1, 0, 0), 6);
    }

    #[test]
    fn auxv_patching() {
        let mut auxv = [[AT_ENTRY, 1], [AT_BASE, 0]];
        assert!(auxv_set(&mut auxv, AT_BASE, 0x5000));
        assert!(!auxv_set(&mut auxv, AT_EXECFN, 1));
        assert_eq!(auxv_get(&auxv, AT_BASE), Some(0x5000));
        assert_eq!(auxv_get(&auxv, AT_EXECFN), None);
    }

    #[test]
    fn env_entries() {
        assert_eq!(env_key(b"HOME=/root"), b"HOME");
        assert_eq!(env_key(b"NOEQUALS"), b"NOEQUALS");
        assert_eq!(env_value(b"HOME=/root", b"HOME"), Some(&b"/root"[..]));
        assert_eq!(env_value(b"HOMEX=/root", b"HOME"), None);
        assert_eq!(env_value(b"HOME", b"HOME"), None);
    }
}
