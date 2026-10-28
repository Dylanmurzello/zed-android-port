# Resilience for Bun-binary `/etc/resolv.conf` hex-patch

**Status:** Deferred. Active rewrite described under [DNS in the zdroid-bootstrap README](https://github.com/ilkersigirci/zdroid-bootstrap#dns) works today; this doc captures the failure modes when it eventually breaks and the options for replacing it.

## Why this is fragile

Our DNS-survival strategy for Bun-compiled CLIs (claude, codex, future tools) is byte-substring rewriting in the binary's `.rodata`:

```
\x00/etc/resolv.conf\x00 -> \x00/sdcard/.zed/r\x00\x00\x00
```

Equal-length, in-place. Works because Bun statically links c-ares with that exact literal at compile time. Pinned to:

1. The literal being a contiguous, uncompressed byte sequence in the binary.
2. The literal being null-anchored on both sides.
3. c-ares (or whatever DNS resolver Bun ships) using `fopen()` on that path string at runtime.

Any of those three changing in a future Bun release silently breaks DNS for affected CLIs. No compile-time check, no install-time check, no runtime alarm — `npm install` succeeds, `claude` runs, every API call errors with `dns lookup failed` or hangs. Users bounce, we find out via a bug report.

## Failure modes

| Upstream change | Effect on our patch |
| --- | --- |
| Bun renames literal (`/etc/resolv.conf` → `/etc/resolv-conf` or escapes it) | grep miss → 0 matches → silent no-op |
| Bun compresses `.rodata` (zstd at link time) | grep miss → silent no-op |
| Bun switches DNS path (use Android `getaddrinfo()` instead of c-ares) | The patch finds no match because the literal isn't there. Whether DNS works depends on what Bun chose: bionic `getaddrinfo` works fine, hardcoded UDP-to-8.8.8.8 also works, hardcoded UDP-to-127.0.0.1 fails |
| Bun ships its own resolver with a different config path (`/etc/resolv.toml`?) | grep miss; new pattern needed |

In all cases: no log line emitted by `zed-launcher-gen`, install reports success, runtime DNS fails.

## Detection (the cheap part — do this first when it bites)

Add a smoke test to the v0.1.x release cycle:

```sh
# in script/release.sh, after gh release create:
adb install -r Zdroid-X.Y.Z.apk
adb shell am start -n com.zdroid/.MainActivity
sleep 30
# inside an integrated terminal session, but scripted via adb shell input:
adb shell input text "npm install -g @anthropic-ai/claude-code && claude --version"
adb shell input keyevent 66
sleep 60
# scrape logcat for "dns lookup failed" or claude exit code != 0
adb logcat -d | grep -E "claude.*dns|claude.*ECONN|claude.*timeout" && exit 1
```

Cheap, catches the regression before users do. Fails the release if the smoke test fails. Add to v0.1.2 or whenever the next release lands.

## Alternative interception layers (the deep fix)

### Option A: LD_PRELOAD on `getaddrinfo()`

Compile a small `libzed-resolv.so` via NDK clang. Implements `getaddrinfo()` and forwards to bionic's after rewriting any path lookup that targets `/etc/resolv.conf`. Chained into LD_PRELOAD via the npm wrapper.

**Limitation:** only works for **dynamically-linked** binaries. Bun-compiled CLIs link musl statically; their `getaddrinfo()` is baked into the binary, no PLT/GOT entry to intercept. **This option does not help for the Bun case** — it would only catch glibc-dynamic or musl-dynamic CLIs that happen to also bake `/etc/resolv.conf`. See [`deferred-ld-preload-shim.md`](deferred-ld-preload-shim.md) for the same dropped-then-revisited reasoning at the `open()` layer.

### Option B: FUSE-mount `/etc/resolv.conf` from userspace

Run a small FUSE daemon that exposes a single file at `/etc/resolv.conf` containing live nameservers. Then patches become unnecessary — every binary's `fopen("/etc/resolv.conf")` resolves to our file.

**Constraint:** Android's FUSE handler restricts mount points. Mounting `/etc/` requires CAP_SYS_ADMIN which `untrusted_app_27` doesn't have. Unprivileged user namespaces (which Linux uses for unprivileged FUSE) are gated by SELinux on Android. Worth verifying empirically on a current device — if Android 14+ relaxed any of this, FUSE becomes viable.

### Option C: Network-namespace + bind-mount inside it

`unshare --net --mount`, then `mount --bind $PREFIX/etc/resolv.conf /etc/resolv.conf` inside the new namespace. Run claude there.

**Constraint:** unshare needs CAP_SYS_ADMIN or unprivileged user-ns support. Android blocks both. Same wall as B.

### Option D: ptrace / seccomp-bpf

Intercept the `openat()` syscall and rewrite the path argument before kernel. This is essentially what proot did, and the reason we abandoned proot was the per-syscall ptrace overhead (5+ minute claude startup, mouse disappearing, claude's Statsig client retrying thousands of DNS queries per second). seccomp-bpf is faster than ptrace but still gates user-space, and the Statsig storm would still bottleneck.

**Worth revisiting only if** a newer Bun stops the Statsig retry loop and the ptrace tax becomes manageable.

### Option E: Maintain a pattern dictionary, automate detection

Keep a registry of `(bun_version, resolv_pattern, comment)` entries. The `zed-launcher-gen` script tries each pattern in order; first match wins. New Bun releases land → smoke test fails → contributor adds an entry to the dict, ships v0.1.X+1.

Reactive but cheap. Likely the right choice for v0.x given that none of A–D are actually viable on stock Android-untrusted-app permissions.

## Recommendation

1. **First** when it breaks: add the smoke test (Detection section above). Confirms the failure mode.
2. **Then**: dump the binary's strings, find the new resolver-config path, add a new entry to the launcher-gen patcher.
3. **Long-term**: track Android version capabilities — if unprivileged user namespaces ever get unblocked on `untrusted_app_*`, Option B or C becomes the durable fix.

## Triggering events to revisit

- User reports "claude can't reach the API" / `dns lookup failed` after a `npm install -g @anthropic-ai/claude-code` upgrade.
- New Bun major release notes mention DNS / resolver / link-time changes.
- A claude-code package release between two known-good versions starts misbehaving on Zdroid only.

In any of those cases: pull the broken binary, run `strings | grep -iE 'resolv|dns'`, compare against the pattern dict, write the fix.
