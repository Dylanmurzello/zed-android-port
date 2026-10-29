//! Guest-to-host path bindings. A binding maps a guest directory prefix
//! to a host directory; the rest of the path is appended unchanged.
//! Lookups pick the longest matching prefix on a component boundary.

use crate::path::{is_absolute, strip_prefix};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding<'a> {
    pub guest: &'a [u8],
    pub host: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingsError {
    TooMany,
    NotAbsolute,
    Empty,
}

pub const MAX_BINDINGS: usize = 32;

/// Fixed-capacity binding table. Guest paths `/dev`, `/proc` and `/sys`
/// are always identity-mapped: the supervisor adds everything else
/// through [`Bindings::parse_spec`].
#[derive(Debug, Clone, Copy)]
pub struct Bindings<'a> {
    entries: [Binding<'a>; MAX_BINDINGS],
    len: usize,
}

pub const ALWAYS_IDENTITY: &[&[u8]] = &[b"/dev", b"/proc", b"/sys"];

impl<'a> Bindings<'a> {
    pub const fn empty() -> Self {
        Self {
            entries: [Binding {
                guest: b"",
                host: b"",
            }; MAX_BINDINGS],
            len: 0,
        }
    }

    /// Table with the always-identity entries already present.
    pub fn with_defaults() -> Self {
        let mut bindings = Self::empty();
        for guest in ALWAYS_IDENTITY {
            // Cannot fail: the table is empty and the paths are absolute.
            let _ = bindings.push(guest, guest);
        }
        bindings
    }

    pub fn push(&mut self, guest: &'a [u8], host: &'a [u8]) -> Result<(), BindingsError> {
        if guest.is_empty() || host.is_empty() {
            return Err(BindingsError::Empty);
        }
        if !is_absolute(guest) || !is_absolute(host) {
            return Err(BindingsError::NotAbsolute);
        }
        if self.len == MAX_BINDINGS {
            return Err(BindingsError::TooMany);
        }
        self.entries[self.len] = Binding { guest, host };
        self.len += 1;
        Ok(())
    }

    /// Parse `guest:host;guest2;guest3:host3`. A bare `guest` is mapped
    /// to itself. Empty entries are ignored.
    pub fn parse_spec(&mut self, spec: &'a [u8]) -> Result<(), BindingsError> {
        for entry in spec.split(|&byte| byte == crate::LIST_SEPARATOR as u8) {
            if entry.is_empty() {
                continue;
            }
            match entry.iter().position(|&byte| byte == crate::BIND_SEPARATOR as u8) {
                Some(index) => self.push(&entry[..index], &entry[index + 1..])?,
                None => self.push(entry, entry)?,
            }
        }
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Binding<'a>> {
        self.entries[..self.len].iter()
    }

    /// Longest binding whose guest prefix covers `guest`, with the
    /// remainder of the path.
    pub fn match_guest<'p>(&self, guest: &'p [u8]) -> Option<(&Binding<'a>, &'p [u8])> {
        self.longest(guest, |binding| binding.guest)
    }

    /// Longest binding whose host prefix covers `host`, with the
    /// remainder of the path.
    pub fn match_host<'p>(&self, host: &'p [u8]) -> Option<(&Binding<'a>, &'p [u8])> {
        self.longest(host, |binding| binding.host)
    }

    fn longest<'p>(
        &self,
        path: &'p [u8],
        side: impl Fn(&Binding<'a>) -> &'a [u8],
    ) -> Option<(&Binding<'a>, &'p [u8])> {
        let mut best: Option<(&Binding<'a>, &'p [u8])> = None;
        for binding in self.iter() {
            if let Some(rest) = strip_prefix(side(binding), path) {
                let better = match best {
                    Some((current, _)) => side(binding).len() > side(current).len(),
                    None => true,
                };
                if better {
                    best = Some((binding, rest));
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_identity() {
        let bindings = Bindings::with_defaults();
        let (binding, rest) = bindings.match_guest(b"/dev/pts/3").unwrap();
        assert_eq!(binding.host, b"/dev");
        assert_eq!(rest, b"/pts/3");
        assert!(bindings.match_guest(b"/devices").is_none());
    }

    #[test]
    fn spec_parsing_and_longest_match() {
        let mut bindings = Bindings::with_defaults();
        bindings
            .parse_spec(b"/tmp:/data/user/0/com.zdroid/files/tmp;/data/user/0/com.zdroid/files;;/dev/shm:/data/user/0/com.zdroid/files/tmp/shm")
            .unwrap();
        let (binding, rest) = bindings.match_guest(b"/dev/shm/sem.x").unwrap();
        assert_eq!(binding.host, b"/data/user/0/com.zdroid/files/tmp/shm");
        assert_eq!(rest, b"/sem.x");
        let (binding, rest) = bindings.match_guest(b"/tmp").unwrap();
        assert_eq!(binding.host, b"/data/user/0/com.zdroid/files/tmp");
        assert_eq!(rest, b"");
        let (binding, _) = bindings
            .match_guest(b"/data/user/0/com.zdroid/files/home/projects")
            .unwrap();
        assert_eq!(binding.guest, binding.host);
    }

    #[test]
    fn host_lookup_prefers_longest() {
        let mut bindings = Bindings::with_defaults();
        bindings
            .parse_spec(b"/tmp:/data/user/0/com.zdroid/files/tmp;/data/user/0/com.zdroid/files")
            .unwrap();
        let (binding, rest) = bindings
            .match_host(b"/data/user/0/com.zdroid/files/tmp/x")
            .unwrap();
        assert_eq!(binding.guest, b"/tmp");
        assert_eq!(rest, b"/x");
    }

    #[test]
    fn rejects_bad_entries() {
        let mut bindings = Bindings::empty();
        assert_eq!(bindings.parse_spec(b"tmp:/x"), Err(BindingsError::NotAbsolute));
        assert_eq!(bindings.parse_spec(b"/tmp:"), Err(BindingsError::Empty));
    }
}
