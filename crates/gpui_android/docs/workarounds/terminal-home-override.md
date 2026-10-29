# Terminal HOME override

**Status:** Superseded

> Superseded by `rootfs-guest-env.md`: the terminal copies the process
> `HOME` and the loader applies the guest view at exec.

crates/terminal/src/terminal.rs calls zdroid_hooks::insert_terminal_env on Android, which overrides HOME with TERMUX__HOME for the integrated-terminal subprocess only. See home-env-dual-pointing.md for the why.

**Detailed writeup: TODO.** Stub created so the index links resolve.
