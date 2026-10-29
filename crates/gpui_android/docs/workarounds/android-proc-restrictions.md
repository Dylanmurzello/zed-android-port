# The `/proc` files the app sandbox refuses

**Status:** Active
**Phase / Commit:** engine, found through the 0.4.0 release smoke (2026-10-28)
**Files:** `crates/zd_engine_core/src/procfs.rs` (`SynthFile`, `classify`, `constant_content`, `cpu_list`), `crates/zd_engine_core/src/translate.rs` (`Translated::ProcSynth`), `crates/zd_loader/src/procfs.rs` (`serve`, `render`), `crates/zd_loader/src/handler.rs` (`path_syscall`, `serve_bytes`), `crates/zd_loader/differential/golden.sh` ("proc files")

## Problem

`npm install -g @anthropic-ai/claude-code` ended with npm's "Exit handler
never called" and no postinstall, while the same command in an Ubuntu
26.04 container with the same node and npm installed in 11 s. The trace
showed npm never spawning anything: its script queue takes its
concurrency from `os.cpus().length`, which was 0 in the guest, and a
queue with a limit of 0 starts nothing, so its promise never settled and
node exited when the loop ran dry. `os.cpus()` was empty because libuv
counts the `cpu<n>` lines of `/proc/stat`, and `/proc/stat` is EACCES on
open and invisible to stat for an untrusted app.

## Constraint

Measured from the guest on Android 16 (SELinux domain `untrusted_app`): an
app may read `/proc/cpuinfo`, `/proc/meminfo`, `/proc/mounts`,
`/proc/sys/kernel/random/{boot_id,uuid}` and its own `/proc/self/*`.
Everything else is refused: `stat`, `loadavg`, `uptime`, `vmstat`,
`version`, `diskstats`, `swaps`, `filesystems`, `cmdline`, `modules`,
`partitions`, the whole of `/proc/net/*` and `/proc/sys/*` (the
directories themselves are visible). Linux software takes these for
granted: libuv and Go read `/proc/stat`, `uptime` reads `loadavg` and
`uptime`, build tools read `pid_max` and `file-max`, netstat reads
`/proc/net`.

## Solution

The translator recognises the catalog (`procfs::classify`, exact guest
paths) and the handler, for every path syscall on one of them, issues
the kernel's own syscall first and only when that answers EACCES or
ENOENT serves the engine's version: `openat`/`openat2` get a memfd with
the rendered text (read-only; write access is EACCES as on procfs),
`newfstatat`/`statx` a root-owned `0444` regular file of size 0 with a
fixed inode per file, `faccessat` yes for read and no for write or
execute, `readlinkat` EINVAL. Content comes from what the sandbox allows:
`/proc/stat` lists one zero-jiffies line per cpu in
`/sys/devices/system/cpu/online` plus `btime` from the realtime and boot
clocks and the process count from `sysinfo(2)`; `loadavg` and `uptime`
come from `sysinfo(2)`; `version` and the `sys/kernel/{osrelease,
hostname,version}` knobs from `uname(2)` with the engine's node-name and
release overrides applied; `vmstat` from `sysinfo(2)` memory figures;
the rest are fixed texts (filesystem list, empty tables with their
headers, the usual sysctl defaults with a generous `pid_max`).

Counters the sandbox hides read as zero: `top` and `htop` show 0% cpu,
`/proc/net/dev` shows only `lo`. That is the honest view of what the
process can see, and every reader keeps working.

## Why not the obvious fix

Patching npm (`--foreground-scripts`) or node would be the Termux path
this engine exists to avoid, and the same hole would break the next
reader of `/proc/stat`. A bind mount or a fake `/proc` directory in the
rootfs cannot carry live values, and the kernel's file must stay the
answer wherever it is allowed.

## Failure mode

A tool reporting zero cpus, `uptime` failing to open `/proc/loadavg`, or
the "proc files" section of the differential suite diverging from the
Ubuntu container. New readers of a `/proc` file outside the catalog get
the sandbox's EACCES; add the file to `SynthFile`.

## See also

- `rootfs-engine-integration.md`
- `crates/zd_loader/differential/run.sh`: the oracle run
