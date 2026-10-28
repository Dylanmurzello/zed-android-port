# targetSdk=28 for execve from /data/data

**Status:** Active

> Since the rootfs engine the pin is no longer load-bearing for spawns:
> the kernel only execs the loader in the APK's native library dir, which
> every targetSdk may execute. The bump is a separate decision (it also
> makes the notification permission a real prompt).

At targetSdk≥29 SELinux blocks execve on app_data_file. We pin targetSdk=28 to keep `untrusted_app_27` domain semantics where execute_no_trans is allowed. Future-proof escape: Termux's system_linker_exec pattern (run via /system/bin/linker64 <bin>) when we eventually have to bump.

**Detailed writeup: TODO.** Stub created so the index links resolve.
