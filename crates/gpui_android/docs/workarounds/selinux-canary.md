# SELinux context canary log

**Status:** Active
**Phase / Commit:** targetSdk 35 bump (2026-10-28)
**Files:** `crates/gpui_android/src/selinux_canary.rs`, called from `examples/zed_android/src/lib.rs` at boot

## Problem

A spawn failure reads the same from the editor whatever its cause. The
app's SELinux domain decides what the kernel lets its processes exec and
map, and it follows `targetSdk` (`untrusted_app` at 35 on Android 16), so
the domain belongs in the boot log next to the loader path.

## Solution

`check_selinux_context` reads `/proc/self/attr/current` and logs it:

```
selinux_canary: /proc/self/attr/current = Some("u:r:untrusted_app:s0:c...")
```

Nothing is checked: the rootfs engine runs in any untrusted app domain,
since the kernel execs only the loader in the native library dir and the
binaries are mapped.

## See also

- `extract-native-libs-loader.md`: where the loader lives and why
