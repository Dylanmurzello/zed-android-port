# Terminal HOME override

**Status:** Active

crates/terminal/src/terminal.rs calls zdroid_hooks::insert_terminal_env on Android, which overrides HOME with TERMUX__HOME for the integrated-terminal subprocess only. See home-env-dual-pointing.md for the why.

**Detailed writeup: TODO.** Stub created so the index links resolve.
