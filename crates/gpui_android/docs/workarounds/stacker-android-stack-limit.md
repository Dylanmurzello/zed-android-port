# stacker reads the stack limit on Android

**Status:** Active
**Phase / Commit:** Zdroid 0.9.23
**Files:** `crates/gpui_android/examples/zed_android/Cargo.toml` (`[patch.crates-io]`),
[ilkersigirci/stacker](https://github.com/ilkersigirci/stacker/tree/android-stack-limit)
(`src/backends/mod.rs`, `src/backends/unix.rs`)

## Problem

A CPU profile of scrolling a file (2026-10-29, simpleperf with the build's
symbols) found `android_main` spending about 7% of each frame in `mmap`,
`mprotect` and `munmap`, all called from `stacker::_grow`, plus page faults
on the fresh pages, which the profile spreads over ordinary functions.

gpui guards its element recursion (layout, prepaint, paint, and dropping the
previous frame's elements) with `stacksafe`, which the app enables through
gpui's `stacker` feature. `stacksafe` calls `stacker::maybe_grow`, which runs
the call on a new 2 MiB stack when less than 128 KiB of stack remains, and
also when it can't tell how much remains.

## Constraint

stacker 0.1.25 reads a thread's stack limit with `pthread_getattr_np` only on
the targets it lists, and Android isn't one of them, so Android gets its
fallback backend, which reports no limit. `stacker::remaining_stack()` was
always `None` on Android, so every guarded call on the thread's own stack
mapped, switched to and unmapped a new stack, many times a frame. A test
program on the reference hardware printed `None` for both the main thread and
a 1 MiB thread; giving `android_main` an 8 MiB stack changed nothing.

No stacker release supports Android (rust-lang/stacker#21 proposed it and
was closed without merging).

## Solution

Zdroid patches stacker in a fork, one commit on its `master` after 0.1.25,
pinned by revision in the app's `[patch.crates-io]`: Android joins the
targets that use `pthread_getattr_np`, which bionic implements and libc
exposes for Android. With it, the same test program prints the remaining
stack (`Some(8377264)` on the main thread, `Some(1055520)` on a 1 MiB
thread), and stacker's own tests pass on the tablet.

## Why this works

`maybe_grow` only grows when the remaining stack is below the red zone, as on
Linux, so gpui runs on `android_main`'s own stack until its recursion really
gets deep.

## Failure mode if regressed

If the patch is lost, for example when a stacker update replaces the pinned
revision, frames get slower again, and `stacker::_grow` with
`mmap`/`munmap` reappears in a simpleperf profile of scrolling. Drop the
patch once a stacker release supports Android.

## See also

- [Choreographer-driven vsync](choreographer-vsync.md), which records the
  scrolling measurements this came from.
