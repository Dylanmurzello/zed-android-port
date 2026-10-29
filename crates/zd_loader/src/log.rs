//! Diagnostics to stderr and, when `ZD_LOG` names a file, appended
//! there too. `core::fmt` is fine on the startup path; the trap handler
//! uses the byte-level helpers to stay small and reentrant.

use core::fmt;

use crate::config;
use crate::sys;

pub fn write_all(fd: i32, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let written = sys::write(fd, bytes);
        if written <= 0 {
            return;
        }
        bytes = &bytes[written as usize..];
    }
}

/// Append one already-formatted line to the `ZD_LOG` file, if set.
pub fn append_to_log_file(bytes: &[u8]) {
    let config = config::get();
    if config.log.is_empty() {
        return;
    }
    let Ok(fd) = sys::openat(
        sys::AT_FDCWD,
        config.log.as_ptr(),
        sys::O_WRONLY | sys::O_CREAT | sys::O_APPEND | sys::O_CLOEXEC,
        0o600,
    ) else {
        return;
    };
    write_all(fd, bytes);
    if let Err(_errno) = sys::close(fd) {
        // Nothing to report to: the log file is the reporting channel.
    }
}

/// Line writer that buffers into a fixed array so one line reaches
/// stderr and the log file as a single `write` each.
pub struct Line {
    buffer: [u8; 1024],
    len: usize,
}

impl Line {
    pub fn new() -> Self {
        Self {
            buffer: [0; 1024],
            len: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let room = self.buffer.len() - self.len;
        let take = bytes.len().min(room);
        self.buffer[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
    }

    pub fn push_u64(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        self.push(itoa(&mut digits, value));
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer[..self.len]
    }

    pub fn emit(mut self) {
        self.push(b"\n");
        write_all(2, &self.buffer[..self.len]);
        append_to_log_file(&self.buffer[..self.len]);
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push(text.as_bytes());
        Ok(())
    }
}

pub fn itoa(buffer: &mut [u8; 20], mut value: u64) -> &[u8] {
    let mut index = buffer.len();
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    &buffer[index..]
}

/// Report a fatal condition and exit with 126, the shell convention for
/// "found but could not run".
pub fn abort(args: fmt::Arguments<'_>) -> ! {
    abort_code(126, args)
}

/// Report a fatal condition and exit with `code` (127 for "not found").
pub fn abort_code(code: usize, args: fmt::Arguments<'_>) -> ! {
    let mut line = Line::new();
    line.push(b"zdload: ");
    if fmt::write(&mut line, args).is_err() {
        line.push(b"(unformattable)");
    }
    line.emit();
    sys::exit_group(code)
}

#[macro_export]
macro_rules! abort {
    ($($arg:tt)*) => {
        $crate::log::abort(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! emit_line {
    ($($arg:tt)*) => {{
        let mut line = $crate::log::Line::new();
        if ::core::fmt::write(&mut line, format_args!($($arg)*)).is_err() {
            line.push(b"(unformattable)");
        }
        line.emit();
    }};
}

/// Byte slice as text for `{}`; non-UTF-8 bytes print as `?`.
pub struct Bytes<'a>(pub &'a [u8]);

impl fmt::Display for Bytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                f.write_str("?")?;
            }
        }
        Ok(())
    }
}
