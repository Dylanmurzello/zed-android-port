//! Process-level invariants the editor's children depend on.
//!
//! An Android app process is forked from zygote, not started from a
//! terminal, and nothing guarantees it owns fds 0, 1 and 2. When one of
//! them is free, the next `openpty` or pipe lands there, and code that
//! later closes "its" fd number after the standard streams were set up
//! for a child (alacritty's pre-exec hook does exactly that with the pty
//! pair) closes the child's stdin, stdout or stderr instead. bash then
//! starts non-interactive and shows no prompt. Holding `/dev/null` on
//! every missing standard fd keeps fd numbers 0 to 2 out of circulation.
//!
//! The same process starts in the host's `/`, and a child spawned without
//! an explicit directory inherits it. Inside the rootfs engine that
//! directory has no guest spelling (`getcwd` reports it unreachable), so
//! node died in `uv_cwd` before npm could run `npm info`. The files dir is
//! the one directory with the same path on both sides.

use std::ffi::CString;
use std::path::Path;

/// Move the process to `dir`, the directory children without one of their
/// own will run in.
pub fn ensure_working_directory(dir: &Path) {
    if let Err(error) = std::env::set_current_dir(dir) {
        log::warn!(
            "process_hygiene: cannot chdir to {}: {error}",
            dir.display()
        );
    }
}

/// Open `/dev/null` on any of fds 0, 1, 2 that is currently closed.
pub fn ensure_standard_fds() {
    let dev_null = match CString::new("/dev/null") {
        Ok(path) => path,
        Err(_) => return,
    };
    for (fd, flags) in [
        (libc::STDIN_FILENO, libc::O_RDONLY),
        (libc::STDOUT_FILENO, libc::O_WRONLY),
        (libc::STDERR_FILENO, libc::O_WRONLY),
    ] {
        // SAFETY: plain fd queries and opens on constant fd numbers.
        unsafe {
            if libc::fcntl(fd, libc::F_GETFD) != -1 {
                continue;
            }
            let opened = libc::open(dev_null.as_ptr(), flags | libc::O_CLOEXEC);
            if opened < 0 {
                log::warn!("process_hygiene: cannot open /dev/null for fd {fd}");
                continue;
            }
            if opened != fd {
                if libc::dup2(opened, fd) < 0 {
                    log::warn!("process_hygiene: dup2 onto fd {fd} failed");
                }
                libc::close(opened);
            } else {
                // Landed directly on the target: it must not be CLOEXEC,
                // children need it too.
                libc::fcntl(fd, libc::F_SETFD, 0);
            }
            log::info!("process_hygiene: fd {fd} was closed; now /dev/null");
        }
    }
}
