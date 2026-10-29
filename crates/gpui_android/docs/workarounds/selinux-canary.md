# SELinux context canary log

**Status:** Active
**Phase / Commit:** L2 (as the targetSdk 28 gate); reduced to a log line with the targetSdk 35 bump (2026-10-28)
**Files:** `crates/gpui_android/src/selinux_canary.rs`, called from `examples/zed_android/src/lib.rs` at boot

## Problem

A spawn failure reads the same from the editor whatever its cause. The
app's SELinux domain decides what the kernel lets its processes exec and
map, and it follows `targetSdk` (`untrusted_app_27` at 28, `untrusted_app`
at 35 on Android 16), so the domain belongs in the boot log next to the
loader path.

## Solution

`check_selinux_context` reads `/proc/self/attr/current` and logs it:

```
selinux_canary: /proc/self/attr/current = Some("u:r:untrusted_app:s0:c...")
```

In the targetSdk 28 era the function also errored when the domain was not
`untrusted_app_27` or `untrusted_app_25`, because the bootstrap runtime
exec'd files under app data and only those domains allow it. The rootfs
engine execs nothing there (the kernel execs the loader in the native
library dir and the binaries are mapped), so nothing is checked any more.

## See also

- `targetsdk-28-execve.md`: the pin and why it went
- `extract-native-libs-loader.md`: where the loader lives and why
