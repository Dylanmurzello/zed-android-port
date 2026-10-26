//! Byte-level path helpers. No allocation: every writer takes an output
//! buffer and returns the number of bytes written.

/// PATH_MAX on Linux, including the terminating NUL the kernel wants.
pub const MAX_PATH: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The result would not fit the output buffer.
    TooLong,
    /// An absolute path was required.
    NotAbsolute,
}

pub fn is_absolute(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
}

/// Path components with empty entries and `.` skipped. `..` is yielded
/// as is; callers decide what it means.
pub struct Components<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Components<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            while let Some((&b'/', tail)) = self.rest.split_first() {
                self.rest = tail;
            }
            if self.rest.is_empty() {
                return None;
            }
            let end = self
                .rest
                .iter()
                .position(|&byte| byte == b'/')
                .unwrap_or(self.rest.len());
            let (component, tail) = self.rest.split_at(end);
            self.rest = tail;
            if component != b"." {
                return Some(component);
            }
        }
    }
}

pub fn components(path: &[u8]) -> Components<'_> {
    Components { rest: path }
}

/// Number of components after clamping `..` at the root, i.e. the depth
/// of the directory the path names.
pub fn depth(path: &[u8]) -> usize {
    let mut depth = 0usize;
    for component in components(path) {
        if component == b".." {
            depth = depth.saturating_sub(1);
        } else {
            depth += 1;
        }
    }
    depth
}

pub struct Writer<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl<'a> Writer<'a> {
    pub fn new(out: &'a mut [u8]) -> Self {
        Self { out, len: 0 }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), PathError> {
        let end = self.len.checked_add(bytes.len()).ok_or(PathError::TooLong)?;
        // Leave room for the NUL a syscall-bound buffer needs.
        if end >= self.out.len() {
            return Err(PathError::TooLong);
        }
        self.out[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    /// Append one component, inserting a `/` unless the buffer is empty
    /// or already ends with one.
    pub fn push_component(&mut self, component: &[u8]) -> Result<(), PathError> {
        if self.len > 0 && self.out[self.len - 1] != b'/' {
            self.push(b"/")?;
        }
        self.push(component)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.out[..self.len]
    }

    /// NUL-terminate without counting the NUL in the returned length.
    pub fn finish(self) -> usize {
        self.out[self.len] = 0;
        self.len
    }
}

/// Copy an absolute path removing only the `..` components that would
/// climb above the root. Inner `..` stay for the kernel to resolve, since
/// collapsing them lexically is wrong across symlinked directories.
pub fn clamp_to_root(path: &[u8], out: &mut [u8]) -> Result<usize, PathError> {
    if !is_absolute(path) {
        return Err(PathError::NotAbsolute);
    }
    let mut writer = Writer::new(out);
    writer.push(b"/")?;
    let mut depth = 0usize;
    for component in components(path) {
        if component == b".." {
            if depth == 0 {
                continue;
            }
            depth -= 1;
        } else {
            depth += 1;
        }
        writer.push_component(component)?;
    }
    Ok(writer.finish())
}

/// Relative path that reaches absolute `target` from inside absolute
/// directory `from_dir`, lexically. An empty result means "the same
/// directory", written as `.`.
pub fn relative_path(from_dir: &[u8], target: &[u8], out: &mut [u8]) -> Result<usize, PathError> {
    if !is_absolute(from_dir) || !is_absolute(target) {
        return Err(PathError::NotAbsolute);
    }
    let mut common = 0usize;
    {
        let mut from = components(from_dir);
        let mut to = components(target);
        loop {
            match (from.next(), to.next()) {
                (Some(a), Some(b)) if a == b => common += 1,
                _ => break,
            }
        }
    }
    let mut writer = Writer::new(out);
    for _ in components(from_dir).skip(common) {
        writer.push_component(b"..")?;
    }
    for component in components(target).skip(common) {
        writer.push_component(component)?;
    }
    if writer.is_empty() {
        writer.push(b".")?;
    }
    Ok(writer.finish())
}

/// Last component, or the whole input when there is no `/`.
pub fn basename(path: &[u8]) -> &[u8] {
    let trimmed = trim_trailing_slashes(path);
    match trimmed.iter().rposition(|&byte| byte == b'/') {
        Some(index) => &trimmed[index + 1..],
        None => trimmed,
    }
}

/// Directory part: `/a/b` -> `/a`, `/a` -> `/`, `a` -> `.`.
pub fn dirname(path: &[u8]) -> &[u8] {
    let trimmed = trim_trailing_slashes(path);
    match trimmed.iter().rposition(|&byte| byte == b'/') {
        Some(0) => b"/",
        Some(index) => trim_trailing_slashes(&trimmed[..index]),
        None => b".",
    }
}

fn trim_trailing_slashes(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

/// True when `path` is `prefix` or lives under it on a component
/// boundary. Returns the remainder (starting with `/`, or empty).
pub fn strip_prefix<'p>(prefix: &[u8], path: &'p [u8]) -> Option<&'p [u8]> {
    let prefix = trim_trailing_slashes(prefix);
    if prefix == b"/" {
        return if is_absolute(path) { Some(path) } else { None };
    }
    let rest = path.strip_prefix(prefix)?;
    if rest.is_empty() || rest[0] == b'/' {
        Some(rest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clamp(path: &str) -> String {
        let mut out = [0u8; 256];
        let len = clamp_to_root(path.as_bytes(), &mut out).unwrap();
        String::from_utf8(out[..len].to_vec()).unwrap()
    }

    fn rel(from: &str, to: &str) -> String {
        let mut out = [0u8; 256];
        let len = relative_path(from.as_bytes(), to.as_bytes(), &mut out).unwrap();
        String::from_utf8(out[..len].to_vec()).unwrap()
    }

    #[test]
    fn components_skip_dots_and_empties() {
        let got: Vec<&[u8]> = components(b"//usr/./lib//x/..").collect();
        assert_eq!(got, vec![&b"usr"[..], b"lib", b"x", b".."]);
    }

    #[test]
    fn clamp_keeps_inner_dotdot_and_drops_climbs() {
        assert_eq!(clamp("/a/../b"), "/a/../b");
        assert_eq!(clamp("/a/../../b"), "/a/../b");
        assert_eq!(clamp("/../../etc/passwd"), "/etc/passwd");
        assert_eq!(clamp("/"), "/");
        assert_eq!(clamp("/usr/./bin/"), "/usr/bin");
    }

    #[test]
    fn clamp_requires_absolute() {
        let mut out = [0u8; 16];
        assert_eq!(clamp_to_root(b"a/b", &mut out), Err(PathError::NotAbsolute));
    }

    #[test]
    fn clamp_reports_overflow() {
        let mut out = [0u8; 4];
        assert_eq!(clamp_to_root(b"/abcdef", &mut out), Err(PathError::TooLong));
    }

    #[test]
    fn relative_paths() {
        assert_eq!(rel("/usr/bin", "/etc/alternatives/python3"), "../../etc/alternatives/python3");
        assert_eq!(rel("/usr/bin", "/usr/bin/python3.12"), "python3.12");
        assert_eq!(rel("/", "/bin/sh"), "bin/sh");
        assert_eq!(rel("/a/b", "/a/b"), ".");
        assert_eq!(rel("/a/b/c", "/a"), "../..");
    }

    #[test]
    fn names() {
        assert_eq!(basename(b"/usr/bin/ls"), b"ls");
        assert_eq!(basename(b"ls"), b"ls");
        assert_eq!(basename(b"/usr/bin/"), b"bin");
        assert_eq!(dirname(b"/usr/bin/ls"), b"/usr/bin");
        assert_eq!(dirname(b"/ls"), b"/");
        assert_eq!(dirname(b"ls"), b".");
        assert_eq!(dirname(b"/usr/bin/"), b"/usr");
    }

    #[test]
    fn prefix_on_component_boundary() {
        assert_eq!(strip_prefix(b"/dev", b"/dev"), Some(&b""[..]));
        assert_eq!(strip_prefix(b"/dev", b"/dev/pts/0"), Some(&b"/pts/0"[..]));
        assert_eq!(strip_prefix(b"/dev", b"/devices"), None);
        assert_eq!(strip_prefix(b"/", b"/anything"), Some(&b"/anything"[..]));
        assert_eq!(strip_prefix(b"/dev/", b"/dev/null"), Some(&b"/null"[..]));
    }

    #[test]
    fn depth_counts_after_clamping() {
        assert_eq!(depth(b"/usr/bin"), 2);
        assert_eq!(depth(b"/usr/../.."), 0);
        assert_eq!(depth(b"/"), 0);
    }
}
