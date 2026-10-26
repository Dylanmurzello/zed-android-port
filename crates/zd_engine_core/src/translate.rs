//! Guest path to host path translation and back.
//!
//! Absolute guest paths are clamped at the guest root, then matched
//! against the bindings; anything unmatched lives under the rootfs.
//! Relative paths are left alone: the kernel resolves them against the
//! process cwd, which the engine keeps equal to the translated guest cwd.

use crate::bindings::Bindings;
use crate::path::{
    MAX_PATH, PathError, Writer, clamp_to_root, components, depth, dirname, is_absolute,
    relative_path, strip_prefix,
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
    /// by the kernel without translation:
    /// - absolute targets inside a binding become the host absolute path
    ///   (identity bindings keep their guest spelling);
    /// - absolute targets under the rootfs become relative to the link's
    ///   directory;
    /// - relative targets keep their form, minus `..` components that
    ///   would climb above the guest root.
    pub fn rewrite_symlink_target(
        &self,
        link: &[u8],
        target: &[u8],
        out: &mut [u8],
    ) -> Result<usize, TranslateError> {
        let link_dir = dirname(link);
        if is_absolute(target) {
            let mut clamped = [0u8; MAX_PATH];
            let len = clamp_to_root(target, &mut clamped)?;
            let target = &clamped[..len];
            if let Some((binding, rest)) = self.bindings.match_guest(target) {
                let mut writer = Writer::new(out);
                writer.push(binding.host)?;
                writer.push(rest)?;
                return Ok(writer.finish());
            }
            return Ok(relative_path(link_dir, target, out)?);
        }
        let mut writer = Writer::new(out);
        let mut remaining_depth = depth(link_dir);
        for component in components(target) {
            if component == b".." {
                if remaining_depth == 0 {
                    continue;
                }
                remaining_depth -= 1;
            } else {
                remaining_depth += 1;
            }
            writer.push_component(component)?;
        }
        if writer.is_empty() {
            writer.push(b".")?;
        }
        Ok(writer.finish())
    }
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
    let who = parts.next()?;
    let file = match parts.next()? {
        b"exe" => ProcFile::Exe,
        b"auxv" => ProcFile::Auxv,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    let who = proc_who(who)?;
    Some((who, file))
}

fn proc_who(who: &[u8]) -> Option<ProcExe> {
    if who == b"self" {
        return Some(ProcExe::SelfProcess);
    }
    if who == b"thread-self" {
        return Some(ProcExe::ThreadSelf);
    }
    let mut pid: u32 = 0;
    for &digit in who {
        if !digit.is_ascii_digit() {
            return None;
        }
        pid = pid.checked_mul(10)?.checked_add((digit - b'0') as u32)?;
    }
    Some(ProcExe::Pid(pid))
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

    fn symlink(link: &str, target: &str) -> String {
        let mut out = [0u8; 512];
        let len = translator()
            .rewrite_symlink_target(link.as_bytes(), target.as_bytes(), &mut out)
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
        assert_eq!(symlink("/etc/mtab", "../proc/self/mounts"), "../proc/self/mounts");
        assert_eq!(symlink("/x", "../../etc/passwd"), "etc/passwd");
        assert_eq!(symlink("/a/b/x", "../../../../etc"), "../../etc");
        assert_eq!(symlink("/etc/resolv.conf", "/dev/null"), "/dev/null");
        assert_eq!(symlink("/home/me/tmp", "/tmp"), "/data/user/0/com.zdroid/files/tmp");
        assert_eq!(symlink("/root/proj", "/data/data/com.zdroid/files/home/proj"), "/data/user/0/com.zdroid/files/home/proj");
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
