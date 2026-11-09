# LD_PRELOAD libzed-compat.so path-redirect shim (dropped)

**Status:** Dropped — superseded by [hex-patch-resolv-conf.md](https://github.com/Dylanmurzello/zdroid-bootstrap/blob/main/docs/hex-patch-resolv-conf.md)

Originally proposed as the proot-replacement: a small
`libzed-compat.so` we'd compile via NDK clang at cargo build time,
ship in APK assets, extract to `$PREFIX/lib/`, and chain into the
LD_PRELOAD env so dynamic binaries' `open(/etc/resolv.conf)` got
hooked at the C-symbol layer.

## Why we dropped it

LD_PRELOAD only works against **dynamically-linked** binaries —
shimming `open()` requires the call to go through PLT/GOT. Bun-
compiled tools (claude, codex, every npm CLI we care about) link musl
libc **statically**: their `open()` calls go direct to syscall, no
intermediate symbol resolution, **invisible to LD_PRELOAD**.

So the shim would only have helped a marginal class of binaries
(dynamic-musl or dynamic-glibc with hardcoded `/etc/*` paths). For the
common case (static-musl Bun), it was useless from the start.

## What replaced it

The hex-patch approach (L4g) rewrites the `/etc/resolv.conf` literal
in the binary's `.rodata` directly. Works regardless of static vs
dynamic linking, regardless of compile-time inlining, regardless of
PLT layout. The c-ares `fopen()` reads the patched bytes, opens
`/sdcard/.zed/r`, gets real DNS servers populated by
`gpui_android::dns_bridge`. No proot, no LD_PRELOAD, no shim.

## Cost we avoided

Writing the LD_PRELOAD shim would have meant:

- New C source file in the repo
- Build-script integration (cargo build → NDK clang → asset placement)
- Extract-at-boot logic in `termux_bootstrap.rs`
- Chain into LD_PRELOAD ordering (must come before libtermux-exec
  so its hooks resolve first)
- Test that bionic-linked Termux binaries don't get poisoned by it

~150-200 LOC saved by going direct-to-`.rodata`. The hex-patch is
also more **predictable** — it's a mechanical byte rewrite at install
time rather than a runtime hook that might or might not catch every
call site.

## See also

- [hex-patch-resolv-conf.md](https://github.com/Dylanmurzello/zdroid-bootstrap/blob/main/docs/hex-patch-resolv-conf.md) — the
  replacement
- [jni-dns-bridge.md](jni-dns-bridge.md) — Android-side DNS source
- [sdcard-dot-zed-namespace.md](sdcard-dot-zed-namespace.md) — where
  the patched path points
