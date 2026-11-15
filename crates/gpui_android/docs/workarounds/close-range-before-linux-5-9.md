# Closing inherited descriptors without `close_range`

**Status:** Active
**Phase / Commit:** Zdroid 0.15.0, found on the second test device (2026-11-05)
**Files:** `crates/zd_loader/src/main.rs` (`close_inherited_descriptors`), `crates/zd_loader/src/sys.rs` (`getdents64`), `crates/zd_engine_core/src/sysno.rs` (`GETDENTS64`)

## Problem

On a Galaxy S21 Ultra (Exynos 2100, Android 15, One UI 7, kernel 5.4.242)
every program Zdroid started in the runtime exited with status 126 before
it ran. The first launch's setup reported

```
Installing git, the SSH client and certificates failed: apt-get exited with exit status: 126: zdload: close_range failed: errno 38
```

and the terminal, language servers, git and SSH failed the same way. The
editor itself ran.

## Constraint

The editor's environment sets `ZD_CLOSE_FDS=1` for every program it starts,
so the loader closes every descriptor above 2 before the guest runs: the
editor's process holds graphics descriptors opened without `O_CLOEXEC`,
which programs would otherwise inherit (see `rootfs-guest-env.md`). The
loader did that with one `close_range(3, ~0U, 0)` and aborted when it
failed. `close_range` arrived in Linux 5.9. Android 12 and later run on
older kernels too, when the device launched with an older Android: this
phone launched with Android 11 on 5.4 and kept that kernel through its
updates. There, the call returns ENOSYS (38).

Every other system call the loader issues itself (`sys.rs`) predates 5.4.
The guest's own calls to newer system calls (`openat2`, `faccessat2`,
`clone3`, `close_range`) reach the kernel, get ENOSYS, and glibc falls back,
as it does on any older kernel.

## Solution

`close_inherited_descriptors` still tries `close_range` first. On ENOSYS it
opens `/proc/self/fd`, reads it with `getdents64`, and closes each listed
descriptor above 2 other than the directory's own, which is what glibc's
`closefrom` does without `close_range`. The directory's positions follow
descriptor numbers, so closing entries while reading doesn't skip any.

Checked on the phone over adb, with the loader run from `/data/local/tmp`,
`ZD_ROOTFS=/` and `ZD_CLOSE_FDS=1` around `/system/bin/ls -l /proc/self/fd`:
the previous loader printed `close_range failed: errno 38`; this one closed
the socket adb's shell had leaked as descriptor 27 and left only `ls`'s own
descriptors. In the app, `apt update` in the terminal and **Settings →
Android → Install Essentials** (apt and dpkg installing 120 packages) then
worked.

## Why not the obvious fix

Closing every number from 3 up to the descriptor limit needs no directory
read, but the limit is 32768 for apps, so each program the editor starts,
git included, would cost tens of thousands of system calls through the
app's seccomp filter. Marking the editor's descriptors close-on-exec before
each spawn would race with threads opening new ones.

## Failure mode

`zdload: closing inherited descriptors failed: errno N` from any program
the editor starts: `/proc/self/fd` couldn't be read. If programs start but
inherit descriptors, check `ls -l /proc/self/fd` in the terminal.

## See also

- `rootfs-guest-env.md`: the engine block the editor's environment carries
- `rootfs-engine-integration.md`: how spawns reach the loader
