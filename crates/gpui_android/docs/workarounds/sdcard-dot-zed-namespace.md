# `/sdcard/.zed/` namespace for hex-patched paths

**Status:** Active design choice
**Phase / Commit:** L4g session, decision made before commit
**Files:** referenced in `dns_bridge.rs::RESOLV_CONF_PATH`, generator's
`patch_resolv_conf` perl block

## Problem

The hex-patch approach replaces a `/etc/<file>` literal with a
shorter path that points at a writable location. The replacement
**must be ≤ the original byte width** — we can rewrite the same slot
in `.rodata`, not extend it. And the file at the replacement path has
to be writable by an untrusted Android app.

App-private storage would be ideal architecturally
(`/data/data/com.zdroid/files/...`) but the path is **way too
long** to fit any of the canonical `/etc/*` slots:

```
/etc/resolv.conf  → 16 bytes  (target: ≤ 16)
/etc/hosts        → 10 bytes
/etc/nsswitch.conf → 18 bytes
/etc/passwd       → 11 bytes
/etc/services     → 13 bytes

/data/data/com.zdroid/files/r.conf → 43 bytes  ✗
/data/local/tmp/r.conf                      → 22 bytes  ✗ (and untrusted apps can't write there)
/storage/self/primary/r.conf                → 27 bytes  ✗
/sdcard/r.conf                              → 14 bytes  ✓
```

`/sdcard` is the canonical short symlink Android maintains for
`/storage/emulated/0/`. It's **the only writable path short enough**
to fit the 16-byte slot of `/etc/resolv.conf`. App-private is off the
table without instruction-level binary surgery (~100× more complex,
breaks every Bun release).

## The cleanliness problem

`/sdcard/r.conf` at the root of shared storage looks like crap.
Users browsing files in their Files app would see a random `r.conf`
next to Documents, Downloads, DCIM. Looks unprofessional, like the
app dumped cruft.

## Solution

A hidden subdir: **`/sdcard/.zed/`**. Files inside that namespace are
hidden from default file-manager views (Android convention since
forever, like `.thumbnails` or `.android_secure`). Power users who
toggle "show hidden" see `.zed/` and can clearly attribute it.

```
/sdcard/.zed/r          ← resolv.conf  (14 bytes, fits in 16-byte slot with 2 NUL pad)
```

If we ever need to patch other `/etc/*` paths:

| Original (length) | Slot at `/sdcard/.zed/` | Notes |
|---|---|---|
| `/etc/resolv.conf` (16) | `/sdcard/.zed/r` (14, +2 NUL) ✓ | the one we use |
| `/etc/hosts` (10) | doesn't fit (`.zed/h` is 14) | hosts patches need `/sdcard/h` (10) at root if needed |
| `/etc/nsswitch.conf` (18) | `/sdcard/.zed/nss.conf` (20) doesn't fit, `/sdcard/.zed/n` (14) does ✓ | rare; rare tool care |
| `/etc/services` (13) | `/sdcard/.zed/sv` (14) doesn't fit, `/sdcard/.zed/s` (13) ✓ | rare |
| `/etc/passwd` (11) | `/sdcard/.zed/pw` (15) doesn't fit, `/sdcard/.zed/p` (13) doesn't fit | passwd patches need root path |

For now we only patch `resolv.conf`. Other paths pinpointed only when
a specific tool actually fails on them.

## Trade-offs vs app-private

What we lose by being on `/sdcard`:

- **Other apps with storage perms can read it.** Mitigation: the
  contents are public DNS server IPs. No secrets, no auth tokens.
- **FUSE indirection.** Slightly slower I/O than ext4-direct. For a
  ~50-byte config file read once per process startup, irrelevant.
- **Survives uninstall.** App-private gets cleaned on uninstall;
  `/sdcard/.zed/` persists. Only ~50 bytes of cruft if the user
  uninstalls.

What we gain:

- **Path fits in the binary patch slot.** No instruction-level
  surgery needed, patch is mechanical and idempotent.
- **Same file usable by every patched tool** — claude, codex, future
  Bun-compiled CLIs all read `/sdcard/.zed/r` after their respective
  hex-patches. One config to rule them all.
- **`.zed/` is namespace-clear.** A user who sees the dir knows what
  owns it.

## What we ruled out

- Instruction-level binary patching (rewrite the `open()` syscall site
  to use a different pointer): ~100× the complexity, breaks on every
  Bun version bump.
- Creating `/etc/resolv.conf` via mount-bind: needs `CAP_SYS_ADMIN`,
  untrusted apps don't have it.
- Symlink farm at `/etc/`: that path is read-only.
- LD_PRELOAD shim that intercepts `open()`: can't intercept
  static-musl syscalls (no PLT/GOT).
- proot bind: works but 5+ minute startup under telemetry storm. See
  [DNS in the zdroid-bootstrap README](https://github.com/ilkersigirci/zdroid-bootstrap#dns) for why.

## Future-proofing

If Android ever raises the install floor in a way that breaks
`targetSdk=28` and our process can't read/write `/sdcard` the same
way, we'd need to revisit. Until then, this is the boring-as-it-gets
solution.

## See also

- [DNS in the zdroid-bootstrap README](https://github.com/ilkersigirci/zdroid-bootstrap#dns) — the patcher
- [jni-dns-bridge.md](jni-dns-bridge.md) — what populates the file
- [android-noexec-mount.md](android-noexec-mount.md) — sister story,
  same vintage of "Android filesystems vs Linux assumptions"
