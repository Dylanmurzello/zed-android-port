//! `sockaddr_un` as the kernel lays it out: a 16-bit family followed by
//! up to 108 bytes of path, NUL-terminated when there is room.

pub const AF_UNIX: u16 = 1;
pub const SUN_PATH_MAX: usize = 108;
pub const UNIX_MAX: usize = 2 + SUN_PATH_MAX;

/// The filesystem path a `sockaddr_un` names, if any: not an abstract
/// name (which starts with a NUL), not an unnamed socket, not another
/// family.
pub fn unix_path(address: &[u8]) -> Option<&[u8]> {
    if address.len() < 3 || u16::from_ne_bytes([address[0], address[1]]) != AF_UNIX {
        return None;
    }
    let path = &address[2..address.len().min(UNIX_MAX)];
    let path = match path.iter().position(|&byte| byte == 0) {
        Some(end) => &path[..end],
        None => path,
    };
    if path.is_empty() { None } else { Some(path) }
}

/// Write a `sockaddr_un` naming `path` into `out`; returns the length to
/// hand the kernel, or `None` when the path does not fit.
pub fn write_unix(path: &[u8], out: &mut [u8]) -> Option<usize> {
    let len = 2 + path.len() + 1;
    if path.len() >= SUN_PATH_MAX || out.len() < len {
        return None;
    }
    out[..2].copy_from_slice(&AF_UNIX.to_ne_bytes());
    out[2..2 + path.len()].copy_from_slice(path);
    out[2 + path.len()] = 0;
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_paths_only() {
        let mut address = [0u8; UNIX_MAX];
        let len = write_unix(b"/tmp/x.sock", &mut address).unwrap();
        assert_eq!(len, 2 + 11 + 1);
        assert_eq!(unix_path(&address[..len]), Some(&b"/tmp/x.sock"[..]));
        assert_eq!(unix_path(&address[..len - 1]), Some(&b"/tmp/x.sock"[..]));
        assert_eq!(unix_path(&address[..2]), None);
        let mut abstract_name = [0u8; 8];
        abstract_name[..2].copy_from_slice(&AF_UNIX.to_ne_bytes());
        abstract_name[3] = b'x';
        assert_eq!(unix_path(&abstract_name), None);
        let mut inet = [0u8; 16];
        inet[0] = 2;
        inet[2] = b'/';
        assert_eq!(unix_path(&inet), None);
    }

    #[test]
    fn long_paths_are_refused() {
        let mut out = [0u8; UNIX_MAX];
        assert_eq!(write_unix(&[b'a'; SUN_PATH_MAX], &mut out), None);
        assert!(write_unix(&[b'a'; SUN_PATH_MAX - 1], &mut out).is_some());
        let mut small = [0u8; 8];
        assert_eq!(write_unix(b"/tmp/x", &mut small), None);
    }
}
