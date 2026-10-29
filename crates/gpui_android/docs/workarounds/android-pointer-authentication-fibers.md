# Pointer authentication keys and wasm fibers

**Status:** Active
**Phase / Commit:** targetSdk 35 bump (2026-10-28)
**Files:** `crates/gpui_android/src/pointer_authentication.rs`, `examples/zed_android/src/lib.rs` (first calls in `android_main`)

## Problem

Every project open at targetSdk 35 killed the process a second after the
language servers started: `Fatal signal 4 (SIGILL), code 2 (ILL_ILLOPN)`,
`esr 0x72000000 (PAC Exception)`, on a `tokio-rt-worker` thread, at
`autiasp` in `wasmtime_fiber_switch`. The targetSdk 28 build of the same
library never did.

## Constraint

bionic resets the return-address key (IA) on every thread it creates for
apps targeting Android S (API 31) and later: commit 811d180e8 moved the
reset from the zygote fork to `pthread_create` (ART interpreter frames on
the forked stack failed their return check) and limited it to those targets
because Chrome's sandbox rejects the prctl; dcbacd676 skips it on devices
without PAC. So from API 31 each thread signs with its own IA key.

wasmtime's fiber switch (`wasmtime-internal-fiber`, `stackswitch/aarch64.rs`)
is PAC-correct for one key: `wasmtime_fiber_init` signs the address a new
fiber starts at with `pacia1716` and the stack pointer `autiasp` will see,
and every `wasmtime_fiber_switch` signs its own return address with
`paciasp`. Zed's extension host runs wasm calls on a multi-threaded runtime,
so a fiber created or suspended on one worker is resumed on another, and
the authentication runs with the other worker's key.

Measured with the boot probe (`check_pointer_authentication`, a pointer
signed with `pacia1716` on the main thread and on a fresh thread): at
targetSdk 35 all four keys are enabled on both threads and the signatures
differ; at 28 they agree.

## Solution

`disable_return_address_key` runs first in `android_main`:
`prctl(PR_PAC_SET_ENABLED_KEYS, PR_PAC_APIAKEY, 0)`. The enabled-key mask
is inherited by every thread created afterwards, so `paciasp`, `autiasp`
and `pacia1716` are no-ops on all of the editor's threads, the state the
pre-31 targets had; the IB, DA and DB keys stay on. Threads that existed
before (ART's, the UI thread) keep their keys and never run a fiber. The
probe stays as the canary and logs `signing agrees across threads
(no-op on this core or key)` once the fix is in.

## Why not the obvious fix

Pinning every wasm future to one thread means restructuring the extension
host; signing with a key that is not thread-local (`pacga`, the TODO in
wasmtime's assembly) is upstream work for an environment only Android
creates. The prctl removes the one hardening feature the app never had
before the bump, on the app's own threads only.

## Failure mode

A PAC exception in `wasmtime_fiber_switch` again, or the probe logging
`DIFFERS across threads` with IA still enabled: the prctl failed (logged at
boot) or a thread was created before it ran.

## See also

- `targetsdk-28-execve.md`: the bump
- `async-task-waker-identity.md`: the other cross-thread runtime fault of
  this port
