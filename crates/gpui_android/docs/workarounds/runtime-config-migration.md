# Runtime config location and the not-configured state

**Status:** Superseded

> Superseded by `rootfs-engine-integration.md`: with one runtime there is
> no selection file. Boot deletes `<data>/files/etc/zd-runtime.toml` and
> the old bootstrap prefix; the only runtime state is the rootfs sentinel.
**Phase / Commit:** rootfs engine, chunk 0
**Files:** `crates/zdroid_runtime/src/config.rs` (`RuntimeState`, `RuntimeKind`, `runtime_toml_path`), `crates/gpui_android/examples/zed_android/src/lib.rs` (`active_runtime`)

## Problem

`zd-runtime.toml` lived at `<data>/files/usr/etc/`, inside the bootstrap
prefix, and the bootstrap installer wiped `usr/` on every install. Worse, a
parse failure or an adapter type the build did not know was swallowed at boot
and the app silently fell back to bootstrap defaults, so a user whose config
named a removed adapter got a half-working editor with no hint why.

## Constraint

Devices in the field carry `type = "bootstrap"` or `type = "external_termux"`
files. Those adapters no longer exist, and `serde` would reject the whole file
on an unknown enum variant, which is indistinguishable from corruption.

## Solution

- The file moves to `<data>/files/etc/zd-runtime.toml`, outside any userland.
- `RuntimeKind` is `#[serde(untagged)] { Known(RuntimeId), Legacy(String) }`,
  so unknown types parse into `Legacy` with the original string.
- `RuntimeState::load` reads the new path, then the legacy path, migrating a
  known-type legacy file once (`save` to the new path; the old file is left for
  the cleanup button). Every failure is a `NotConfiguredReason`
  (`Missing`, `Legacy`, `Unparseable`, `Unresolvable`); nothing defaults.
- Boot applies `zdroid_runtime::env::baseline_env` when not configured, logs
  the reason, routes `OpenSettings` to the picker while no runtime is
  selected (`ActiveRuntime` global), clears a stale `terminal.shell` that
  points at a removed adapter's shell, and shows a workspace notification
  with an "Open runtime picker" button for the `Legacy` case.

## Why this works

The reason travels with the state, so every surface (logcat, picker,
notification) says the same thing, and the only way to leave the state is a
user selection written by the picker.

## Failure mode if regressed

A device with a legacy or broken config boots without a terminal shell and
without LSPs, and the picker never opens on its own.

## See also

- [SELinux context canary log](selinux-canary.md)
