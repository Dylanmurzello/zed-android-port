# async-task: one vtable pointer per task waker

**Status:** Upstream. Zed pins `zed-industries/async-task` at `015845a`
("Fix task waker vtable identity"), which carries the same one-line fix,
so this fork doesn't vendor the crate; the bounded drain stays.
**Phase / Commit:** rootfs engine, chunk 2 (remote development check)
**Files:** `crates/gpui_android/vendor/async-task/` (one line in `src/raw.rs::RawTask::run`), `crates/gpui_android/examples/zed_android/Cargo.toml` (`[patch.crates-io] async-task`), `crates/gpui_android/src/platform.rs` (bounded drain in `AndroidPlatform::run`)

## Problem

Opening a remote project over SSH froze the editor the moment ssh asked for a
password: the modal never drew the password field, touches did nothing, and
the main thread sat at full CPU. The SSH connect task was polling the master
ssh process's stdout, getting "no data yet", and being woken again at once,
about seven thousand times a second.

## Constraint

async-io remembers one waker per I/O source. When a poll arrives with a waker
that `Waker::will_wake` does not recognise, it wakes the remembered one before
replacing it, so that no task is ever lost. `will_wake` compares the data
pointer and the vtable pointer. async-task builds the waker it hands to a
running future from `&Self::RAW_WAKER_VTABLE` inside `RawTask::run`, and every
clone of that waker from the pointer stored in the task header, which is a
second reference to the same constant. Rust does not promise one address for
two references to a constant that are instantiated in different crates, and in
the Android build they come out different. The task then wakes itself on every
poll. The Android main loop drained its queue with `try_iter`, which never
ends while a runnable re-enqueues itself, so input and painting never ran.

## Solution

- The vendored async-task builds the run-time waker from the header's vtable
  pointer, the one clones already use, so `will_wake` holds between a task's
  waker and its clones. The Android app's `[patch.crates-io]` points at the
  vendored copy; the root workspace keeps upstream's pinned git revision.
- `AndroidPlatform::run` collects the runnables queued at the start of a pass
  and runs only those. A task that re-enqueues itself still costs CPU, but
  input and painting get their turn every pass.

## Why not the obvious fix

Changing the consumer (async-io) is not possible from here, and the desktop
builds do not show the problem because the two references happen to merge
there. A proxy waker in the executor would fix the symptom for every future
but adds an allocation and an atomic per task; the one-line change in
async-task is where the identity is actually broken. Upstream should take the
same line; until then the vendored copy carries it.

## Failure mode

A future that wakes itself on every poll shows up as a hot main thread with a
responsive window (after the bounded drain) and, in the SSH case, a connection
that never passes "Connecting". Look for a poll loop with `will_wake` false.

## See also

- `remote-development-on-android.md`
- `UPSTREAM_MERGE.md`, "Vendored crates"
