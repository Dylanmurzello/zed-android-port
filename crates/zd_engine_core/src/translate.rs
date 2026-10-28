//! Guest path to host path translation and back.
//!
//! Absolute guest paths are clamped at the guest root, then matched
//! against the bindings; anything unmatched lives under the rootfs.
//! Relative paths are left alone: the kernel resolves them against the
//! process cwd, which the engine keeps equal to the translated guest cwd.

use crate::bindings::Bindings;
use crate::path::{
    MAX_PATH, PathError, Writer, clamp_to_root, components, depth, dirname, is_absolute,
    normalize, relative_path, strip_prefix,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcExe {
    SelfProcess,
    ThreadSelf,
    Pid(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Translated {
    /// A host path of this many bytes was written to the output buffer.
    Host(usize),
    /// The input was relative; pass it to the kernel unchanged.
    Relative,
    /// `/proc/<who>/exe`: the caller substitutes the recorded guest
    /// executable path instead of a file under the rootfs.
    ProcExe(ProcExe),
    /// `/proc/<who>/auxv`: the kernel's copy describes the loader, so
    /// the caller serves the patched vector instead.
    ProcAuxv(ProcExe),
    /// `/proc/<who>/cwd` or `/proc/<who>/fd/<n>` (the first `link_len`
    /// bytes of the input) followed by a `..`: the kernel would climb
    /// from the host directory behind the link, so the caller reads the
    /// link, re-anchors the rest in the guest view and translates again.
    ProcLink { link_len: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslateError {
    TooLong,
}

impl From<PathError> for TranslateError {
    fn from(error: PathError) -> Self {
        match error {
            PathError::TooLong | PathError::NotAbsolute => TranslateError::TooLong,
        }
    }
}

pub struct Translator<'a> {
    pub rootfs: &'a [u8],
    pub bindings: Bindings<'a>,
}

impl<'a> Translator<'a> {
    pub fn new(rootfs: &'a [u8], bindings: Bindings<'a>) -> Self {
        Self { rootfs, bindings }
    }

    /// Translate a guest path for a syscall argument.
    pub fn guest_to_host(&self, guest: &[u8], out: &mut [u8]) -> Result<Translated, TranslateError> {
        if !is_absolute(guest) {
            return Ok(Translated::Relative);
        }
        let mut guest = guest;
        while let Some((link, link_len, rest)) = proc_dir_link(guest) {
            match link {
                // The guest's root is the rootfs, so what follows is a guest
                // absolute path again. A bare link stays the kernel's, which
                // keeps `readlink` answering `/`.
                ProcDirLink::Root if !rest.is_empty() => guest = rest,
                ProcDirLink::Directory if components(rest).any(|part| part == b"..") => {
                    return Ok(Translated::ProcLink { link_len });
                }
                _ => break,
            }
        }
        // Clamp straight into the caller's buffer, then slide the tail
        // right to make room for the host prefix: the handler runs on the
        // faulting thread's stack and cannot afford a second PATH_MAX
        // scratch buffer.
        let len = clamp_to_root(guest, out)?;
        match proc_special(&out[..len]) {
            Some((who, ProcFile::Exe)) => return Ok(Translated::ProcExe(who)),
            Some((who, ProcFile::Auxv)) => return Ok(Translated::ProcAuxv(who)),
            None => {}
        }
        let (prefix, rest_start): (&[u8], usize) = match self.bindings.match_guest(&out[..len]) {
            Some((binding, rest)) => (binding.host, len - rest.len()),
            None if len == 1 => (self.rootfs, len),
            None => (self.rootfs, 0),
        };
        let rest_len = len - rest_start;
        let total = prefix.len() + rest_len;
        if total + 1 > out.len() {
            return Err(TranslateError::TooLong);
        }
        out.copy_within(rest_start..len, prefix.len());
        out[..prefix.len()].copy_from_slice(prefix);
        out[total] = 0;
        Ok(Translated::Host(total))
    }

    /// Map a host path the kernel returned (`getcwd`, `readlink`) back to
    /// the guest's view. Paths under neither the rootfs nor a binding are
    /// returned unchanged.
    pub fn host_to_guest(&self, host: &[u8], out: &mut [u8]) -> Result<usize, TranslateError> {
        let mut writer = Writer::new(out);
        let rootfs_rest = strip_prefix(self.rootfs, host);
        let binding = self.bindings.match_host(host);
        let rootfs_wins = match (rootfs_rest, binding) {
            (Some(_), Some((binding, _))) => self.rootfs.len() >= binding.host.len(),
            (Some(_), None) => true,
            _ => false,
        };
        if rootfs_wins {
            let rest = rootfs_rest.unwrap_or(b"");
            if rest.is_empty() {
                writer.push(b"/")?;
            } else {
                writer.push(rest)?;
            }
        } else if let Some((binding, rest)) = binding {
            writer.push(binding.guest)?;
            writer.push(rest)?;
        } else {
            writer.push(host)?;
        }
        Ok(writer.finish())
    }

    /// Target to store for a symlink the guest creates at `link` (a guest
    /// absolute path) pointing at `target`. Keeps every link resolvable
    /// by the kernel without translation. The kernel resolves a relative
    /// target from the link's host directory, so a relative form is only
    /// meaningful when the link and the target sit in the same host tree
    /// (both under the rootfs, or both under the same binding):
    /// - same tree, absolute target: relative to the link's directory;
    /// - same tree, relative target: kept, minus `..` components that
    ///   would climb above the guest root;
    /// - different trees: the target's host absolute path (identity
    ///   bindings keep their guest spelling), which `readlinkat`
    ///   de-translates back.
    pub fn rewrite_symlink_target(
        &self,
        link: &[u8],
        target: &[u8],
        out: &mut [u8],
        scratch: &mut [[u8; MAX_PATH]; 2],
    ) -> Result<usize, TranslateError> {
        let link_dir = dirname(link);
        let link_tree = self.tree_of(link_dir);
        let [absolute, joined] = scratch;
        let absolute_len = if is_absolute(target) {
            normalize(target, absolute)?
        } else {
            let mut writer = Writer::new(&mut joined[..]);
            writer.push(link_dir)?;
            writer.push_component(target)?;
            let joined_len = writer.finish();
            normalize(&joined[..joined_len], absolute)?
        };
        let target_absolute = &absolute[..absolute_len];
        if self.tree_of(target_absolute) != link_tree {
            return match self.guest_to_host(target_absolute, out)? {
                Translated::Host(len) => Ok(len),
                // Absolute and not a /proc special: always a host path.
                _ => Err(TranslateError::TooLong),
            };
        }
        if is_absolute(target) {
            return Ok(relative_path(link_dir, target_absolute, out)?);
        }
        // A relative target is kept verbatim while it stays inside the
        // tree: the kernel then walks the same directories the guest
        // meant. Once it climbs above the tree's root the host parent is
        // not the guest parent, so it is re-expressed from the normalized
        // target instead.
        let tree_root_depth = match link_tree {
            Tree::Rootfs => 0,
            Tree::Binding { guest, .. } => depth(guest),
        };
        let mut remaining = depth(link_dir).saturating_sub(tree_root_depth);
        let mut escapes = false;
        for component in components(target) {
            if component == b".." {
                if remaining == 0 {
                    escapes = true;
                    break;
                }
                remaining -= 1;
            } else {
                remaining += 1;
            }
        }
        if escapes {
            return Ok(relative_path(link_dir, target_absolute, out)?);
        }
        let mut writer = Writer::new(out);
        writer.push(target)?;
        Ok(writer.finish())
    }

    /// Which host tree a guest absolute path lives in.
    fn tree_of(&self, guest: &[u8]) -> Tree<'a> {
        match self.bindings.match_guest(guest) {
            Some((binding, _)) => Tree::Binding {
                guest: binding.guest,
                host: binding.host,
            },
            None => Tree::Rootfs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tree<'a> {
    Rootfs,
    Binding { guest: &'a [u8], host: &'a [u8] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcFile {
    Exe,
    Auxv,
}

/// `/proc/<who>/exe` and `/proc/<who>/auxv`, the two per-process files
/// whose kernel view describes the loader rather than the guest.
fn proc_special(guest: &[u8]) -> Option<(ProcExe, ProcFile)> {
    let rest = strip_prefix(b"/proc", guest)?;
    let mut parts = components(rest);
    let who = proc_who(parts.next()?)?;
    let mut name = parts.next()?;
    if name == b"task" {
        digits(parts.next()?)?;
        name = parts.next()?;
    }
    let file = match name {
        b"exe" => ProcFile::Exe,
        b"auxv" => ProcFile::Auxv,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((who, file))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcDirLink {
    Root,
    Directory,
}

/// `/proc/<who>[/task/<tid>]/root`, `.../cwd` and `.../fd/<n>`: the
/// per-process links to directories. Returns the kind, the byte length
/// of the link path within `guest` and what follows it (empty, or
/// starting with `/`).
fn proc_dir_link(guest: &[u8]) -> Option<(ProcDirLink, usize, &[u8])> {
    let rest = strip_prefix(b"/proc", guest)?;
    let mut parts = components(rest);
    proc_who(parts.next()?)?;
    let mut name = parts.next()?;
    if name == b"task" {
        digits(parts.next()?)?;
        name = parts.next()?;
    }
    let link = match name {
        b"root" => ProcDirLink::Root,
        b"cwd" => ProcDirLink::Directory,
        b"fd" => {
            name = parts.next()?;
            digits(name)?;
            ProcDirLink::Directory
        }
        _ => return None,
    };
    let link_len = name.as_ptr() as usize + name.len() - guest.as_ptr() as usize;
    Some((link, link_len, &guest[link_len..]))
}

fn digits(part: &[u8]) -> Option<u32> {
    if part.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &digit in part {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(digit - b'0'))?;
    }
    Some(value)
}

fn proc_who(who: &[u8]) -> Option<ProcExe> {
    if who == b"self" {
        return Some(ProcExe::SelfProcess);
    }
    if who == b"thread-self" {
        return Some(ProcExe::ThreadSelf);
    }
    Some(ProcExe::Pid(digits(who)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOTFS: &[u8] = b"/data/user/0/com.zdroid/files/rootfs";
    const FILES: &[u8] = b"/data/user/0/com.zdroid/files";

    fn translator() -> Translator<'static> {
        let mut bindings = Bindings::with_defaults();
        bindings
            .parse_spec(
                b"/data/user/0/com.zdroid/files;/data/data/com.zdroid/files:/data/user/0/com.zdroid/files;/tmp:/data/user/0/com.zdroid/files/tmp;/sdcard;/storage",
            )
            .unwrap();
        Translator::new(ROOTFS, bindings)
    }

    fn to_host(guest: &str) -> String {
        let mut out = [0u8; 512];
        match translator().guest_to_host(guest.as_bytes(), &mut out).unwrap() {
            Translated::Host(len) => String::from_utf8(out[..len].to_vec()).unwrap(),
            other => panic!("expected a host path, got {other:?}"),
        }
    }

    fn to_guest(host: &str) -> String {
        let mut out = [0u8; 512];
        let len = translator().host_to_guest(host.as_bytes(), &mut out).unwrap();
        String::from_utf8(out[..len].to_vec()).unwrap()
    }

    #[test]
    fn proc_directory_links() {
        assert_eq!(to_host("/proc/self/root/usr/bin/ls"), "/data/user/0/com.zdroid/files/rootfs/usr/bin/ls");
        assert_eq!(to_host("/proc/1234/root/tmp/x"), "/data/user/0/com.zdroid/files/tmp/x");
        assert_eq!(to_host("/proc/self/task/77/root/etc"), "/data/user/0/com.zdroid/files/rootfs/etc");
        assert_eq!(to_host("/proc/self/root/"), "/data/user/0/com.zdroid/files/rootfs");
        assert_eq!(to_host("/proc/self/root/../etc"), "/data/user/0/com.zdroid/files/rootfs/etc");
        assert_eq!(
            to_host("/proc/self/root/proc/self/root/etc"),
            "/data/user/0/com.zdroid/files/rootfs/etc"
        );
        assert_eq!(to_host("/proc/self/root"), "/proc/self/root");
        assert_eq!(to_host("/proc/self/cwd/a/b"), "/proc/self/cwd/a/b");
        assert_eq!(to_host("/proc/self/fd/3"), "/proc/self/fd/3");
        assert_eq!(to_host("/proc/self/fd/x/.."), "/proc/self/fd/x/..");
        let translator = translator();
        let mut out = [0u8; 512];
        assert_eq!(
            translator.guest_to_host(b"/proc/self/cwd/../x", &mut out).unwrap(),
            Translated::ProcLink { link_len: 14 }
        );
        assert_eq!(
            translator.guest_to_host(b"/proc/self/fd/3/a/../b", &mut out).unwrap(),
            Translated::ProcLink { link_len: 15 }
        );
        assert_eq!(
            translator.guest_to_host(b"/proc/4/task/5/cwd/..", &mut out).unwrap(),
            Translated::ProcLink { link_len: 18 }
        );
        assert_eq!(
            translator.guest_to_host(b"/proc/self/task/9/exe", &mut out).unwrap(),
            Translated::ProcExe(ProcExe::SelfProcess)
        );
    }

    fn symlink(link: &str, target: &str) -> String {
        let mut out = [0u8; 512];
        let mut scratch = [[0u8; MAX_PATH]; 2];
        let len = translator()
            .rewrite_symlink_target(link.as_bytes(), target.as_bytes(), &mut out, &mut scratch)
            .unwrap();
        String::from_utf8(out[..len].to_vec()).unwrap()
    }

    #[test]
    fn rootfs_paths() {
        assert_eq!(to_host("/usr/bin/ls"), "/data/user/0/com.zdroid/files/rootfs/usr/bin/ls");
        assert_eq!(to_host("/"), "/data/user/0/com.zdroid/files/rootfs");
        assert_eq!(to_host("/../etc/passwd"), "/data/user/0/com.zdroid/files/rootfs/etc/passwd");
        assert_eq!(to_host("/usr/../etc"), "/data/user/0/com.zdroid/files/rootfs/usr/../etc");
    }

    #[test]
    fn bound_paths() {
        assert_eq!(to_host("/dev/null"), "/dev/null");
        assert_eq!(to_host("/proc/self/maps"), "/proc/self/maps");
        assert_eq!(to_host("/tmp/x"), "/data/user/0/com.zdroid/files/tmp/x");
        assert_eq!(to_host("/data/data/com.zdroid/files/home"), "/data/user/0/com.zdroid/files/home");
        assert_eq!(to_host("/sdcard/Download"), "/sdcard/Download");
    }

    #[test]
    fn relative_and_proc_exe() {
        let mut out = [0u8; 64];
        let translator = translator();
        assert_eq!(translator.guest_to_host(b"foo/bar", &mut out).unwrap(), Translated::Relative);
        assert_eq!(
            translator.guest_to_host(b"/proc/self/exe", &mut out).unwrap(),
            Translated::ProcExe(ProcExe::SelfProcess)
        );
        assert_eq!(
            translator.guest_to_host(b"/proc/1234/exe", &mut out).unwrap(),
            Translated::ProcExe(ProcExe::Pid(1234))
        );
        assert_eq!(
            translator.guest_to_host(b"/proc/thread-self/exe", &mut out).unwrap(),
            Translated::ProcExe(ProcExe::ThreadSelf)
        );
        assert!(matches!(
            translator.guest_to_host(b"/proc/self/exe/x", &mut out).unwrap(),
            Translated::Host(_)
        ));
        assert_eq!(
            translator.guest_to_host(b"/proc/self/auxv", &mut out).unwrap(),
            Translated::ProcAuxv(ProcExe::SelfProcess)
        );
        assert_eq!(
            translator.guest_to_host(b"/proc/77/auxv", &mut out).unwrap(),
            Translated::ProcAuxv(ProcExe::Pid(77))
        );
        assert!(matches!(
            translator.guest_to_host(b"/proc/self/maps", &mut out).unwrap(),
            Translated::Host(_)
        ));
    }

    #[test]
    fn back_to_guest() {
        assert_eq!(to_guest("/data/user/0/com.zdroid/files/rootfs/usr/bin"), "/usr/bin");
        assert_eq!(to_guest("/data/user/0/com.zdroid/files/rootfs"), "/");
        assert_eq!(to_guest("/data/user/0/com.zdroid/files/home/p"), "/data/user/0/com.zdroid/files/home/p");
        assert_eq!(to_guest("/data/user/0/com.zdroid/files/tmp/x"), "/tmp/x");
        assert_eq!(to_guest("/system/bin/sh"), "/system/bin/sh");
        assert_eq!(to_guest("/dev/pts/0"), "/dev/pts/0");
    }

    #[test]
    fn symlink_targets() {
        assert_eq!(symlink("/usr/bin/python3", "/etc/alternatives/python3"), "../../etc/alternatives/python3");
        assert_eq!(symlink("/bin", "/usr/bin"), "usr/bin");
        // /proc is bound from the host: a relative target would land in the
        // rootfs's empty proc mount point when the kernel resolves it.
        assert_eq!(symlink("/etc/mtab", "../proc/self/mounts"), "/proc/self/mounts");
        assert_eq!(symlink("/x", "../../etc/passwd"), "etc/passwd");
        assert_eq!(symlink("/a/b/x", "../../../../etc"), "../../etc");
        assert_eq!(symlink("/etc/resolv.conf", "/dev/null"), "/dev/null");
        assert_eq!(symlink("/home/me/tmp", "/tmp"), "/data/user/0/com.zdroid/files/tmp");
        assert_eq!(symlink("/root/proj", "/data/data/com.zdroid/files/home/proj"), "/data/user/0/com.zdroid/files/home/proj");
    }

    #[test]
    fn symlink_targets_across_trees() {
        // A link in the bound /tmp cannot reach the rootfs relatively:
        // its host directory is <files>/tmp, which has no sibling etc.
        assert_eq!(symlink("/tmp/hn", "/etc/hostname"), "/data/user/0/com.zdroid/files/rootfs/etc/hostname");
        assert_eq!(symlink("/tmp/x", "../etc/hostname"), "/data/user/0/com.zdroid/files/rootfs/etc/hostname");
        assert_eq!(symlink("/tmp/a", "/tmp/b"), "b");
        assert_eq!(symlink("/tmp/d/a", "../b"), "../b");
        assert_eq!(symlink("/dev/shm/s", "/tmp/y"), "/data/user/0/com.zdroid/files/tmp/y");
        assert_eq!(symlink("/usr/lib/x", "../../tmp/z"), "/data/user/0/com.zdroid/files/tmp/z");
        // Climbs out of the binding and back in: re-expressed so it does
        // not depend on the binding's host directory name.
        assert_eq!(symlink("/tmp/back", "../../tmp/hn"), "hn");
        assert_eq!(symlink("/tmp/d/e/x", "../../d/y"), "../../d/y");
        assert_eq!(symlink("/usr/lib/x", "./a/../b"), "./a/../b");
    }

    #[test]
    fn round_trip_rootfs_paths() {
        for guest in ["/usr/lib/x.so", "/", "/etc", "/tmp/a", "/dev/shm", "/data/user/0/com.zdroid/files/home"] {
            let mut host = [0u8; 512];
            let len = match translator().guest_to_host(guest.as_bytes(), &mut host).unwrap() {
                Translated::Host(len) => len,
                other => panic!("{other:?}"),
            };
            let mut back = [0u8; 512];
            let back_len = translator().host_to_guest(&host[..len], &mut back).unwrap();
            assert_eq!(std::str::from_utf8(&back[..back_len]).unwrap(), guest, "round trip of {guest}");
        }
        let _ = FILES;
    }
}
