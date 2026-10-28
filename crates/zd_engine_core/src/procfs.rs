//! Parsers for the `/proc` files the handler consults.

/// The `TracerPid` field of a `/proc/<pid>/status` (or `thread-self`)
/// file: 0 when nothing traces the task, else the tracer's pid. `None`
/// when the field is missing from the bytes given.
pub fn tracer_pid(status: &[u8]) -> Option<u32> {
    let key = b"TracerPid:";
    let line = status
        .split(|&byte| byte == b'\n')
        .find(|line| line.starts_with(key))?;
    let mut value = 0u32;
    let mut digits = 0;
    for &byte in &line[key.len()..] {
        match byte {
            b' ' | b'\t' if digits == 0 => continue,
            b'0'..=b'9' => {
                value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
                digits += 1;
            }
            _ => break,
        }
    }
    (digits > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::tracer_pid;

    #[test]
    fn reads_the_tracer_field() {
        let status = b"Name:\tstrace\nUmask:\t0022\nState:\tt (tracing stop)\nTgid:\t16259\n\
            Ngid:\t0\nPid:\t16259\nPPid:\t16258\nTracerPid:\t16256\nUid:\t10368\t10368\n";
        assert_eq!(tracer_pid(status), Some(16256));
        assert_eq!(tracer_pid(b"Name:\tbash\nTracerPid:\t0\nUid:\t0\n"), Some(0));
        assert_eq!(tracer_pid(b"TracerPid: 7"), Some(7));
    }

    #[test]
    fn missing_or_empty_field_is_none() {
        assert_eq!(tracer_pid(b"Name:\tbash\nUid:\t0\n"), None);
        assert_eq!(tracer_pid(b"TracerPid:\t\n"), None);
        assert_eq!(tracer_pid(b""), None);
    }
}
