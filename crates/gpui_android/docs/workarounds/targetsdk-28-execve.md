# targetSdk=28 for execve from /data/data

**Status:** Active

## Why

From Android 10, an app that targets API 29 or higher can't `execve` files in its own data directory: SELinux denies `execute_no_trans` on `app_data_file` in the newer `untrusted_app` domains ([Termux execution environment](https://github.com/termux/termux-packages/wiki/Termux-execution-environment), [termux-app#1072](https://github.com/termux/termux-app/issues/1072)). Everything Zdroid runs locally lives there: the Bootstrap userland under `$PREFIX/bin`, the agents, and `zed-askpass-helper`. At targetSdk ≥ 29 every terminal, task, language server and local agent fails to start with `EACCES`.

targetSdk=28 keeps the process in `untrusted_app_27`, which still allows it. The [SELinux canary](selinux-canary.md) logs loudly if a build lands in a different domain.

Termux's own F-Droid and GitHub builds target 28 for the same reason.

## What it costs

- Android warns that the app was "built for an older version of Android" when it's installed.
- Since Android 14, Android won't install an app below a minimum target: 23 on Android 14, 24 on Android 15 and 16 ([minimum targetSdk matrix](https://bayton.org/android/android-minimum-targetsdk-matrix/)). An installed app keeps working through OS updates. If that floor gets close to 28, this decision has to be revisited.
- In-app updates always ask the user to confirm. Updating without asking needs a newer target: API 29 on Android 12, 30 on 13, 31 on 14, rising with later versions ([`SessionParams.setRequireUserAction`](https://developer.android.com/reference/kotlin/android/content/pm/PackageInstaller.SessionParams)).
- Behavior changes Android gates on newer targets don't apply: scoped storage (30), the notification permission prompt (33), foreground service types (34) and enforced edge-to-edge (35). Raising the target means adopting each of them.

## The way out, if it's ever needed

Termux's Google Play build uses `system_linker_exec`: every program runs as `/system/bin/linker64 <program>`, and an `LD_PRELOAD` wrapper does the same for child processes, so the kernel only ever executes the system linker ([termux-exec](https://github.com/termux-play-store/termux-exec)). For Zdroid that's a rework of how the runtime starts processes, and it breaks:

- static binaries, which the linker can't load: `zed-askpass-helper`, and the musl loader that runs Claude Code and Codex;
- programs that read `/proc/self/exe`, which then points at the linker: Bun-compiled tools like Claude Code look for their bundled code there.

Revisit if Android's minimum install target approaches 28, or if Termux's main builds move off 28.
