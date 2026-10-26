# JNI DNS bridge

**Status:** Active
**Phase / Commit:** L4f (`66c979444b`); writer removed with the bootstrap adapter
**Files:** `crates/gpui_android/src/dns_bridge.rs::android_dns_servers`, `MainActivity.kt::getActiveDnsServers()`

> The `/sdcard/.zed/r` writer described below is gone. The JNI query stays
> and seeds the guest rootfs `/etc/resolv.conf` (rootfs engine).

## Problem

The hex-patch flow (see [hex-patch-resolv-conf.md](https://github.com/Dylanmurzello/zdroid-bootstrap/blob/main/docs/hex-patch-resolv-conf.md))
rewrites Bun-compiled CLIs' `/etc/resolv.conf` literal to
`/sdcard/.zed/r`. That file has to **exist**, with real
`nameserver <IP>` lines, for c-ares to find anything. Without it, queries
fall back to localhost (127.0.0.1) → no DNS service → all API calls fail.

## Constraint

We could ship a static `nameserver 8.8.8.8` file and call it done. But
that:

1. Misses Android's actual DNS config — corporate networks, Pi-hole on
   home WiFi, mobile carrier DNS, Private DNS over TLS — all bypassed.
2. Goes stale when user switches WiFi / mobile / VPN.
3. Routes traffic away from the user's chosen DNS provider, possibly
   violating their policy.

The right move is to mirror **whatever Android is actually using** for
DNS at any moment.

## Solution

JNI bridge: a Kotlin method `MainActivity.getActiveDnsServers()` that
calls `ConnectivityManager.getActiveNetwork()` →
`getLinkProperties(network).dnsServers`, formats as a comma-joined
string. Rust side (`gpui_android::dns_bridge::populate_resolv_conf`)
calls it via JNI and writes:

```
nameserver fd4b:cba4:2b5::1
nameserver 192.168.1.1
```

(or whatever IPs Android reports — IPv4 + IPv6 both supported).

Falls back to public DNS (`1.1.1.1`, `8.8.8.8`) if
`ConnectivityManager` reports no active network — happens during early
boot before WiFi attaches, or when offline. The patched binary still
gets a valid file rather than `ENOENT`.

Triggered at boot from `lib.rs::android_main` after the bootstrap
extract + storage permission grant. Idempotent — safe to call
repeatedly (e.g. on `MainEvent::ConfigChanged` if we ever wire that).

## Why this works

- `ConnectivityManager` IS Android's authoritative DNS source. Calling
  through it = matching `getaddrinfo()` would have given us via bionic.
- The patched binary does outbound UDP to those IPs directly. Same
  packet path as bionic would have done — just bypasses bionic's
  resolver wrapper.
- Falls back to public DNS = "graceful degradation": the tool launches
  but might resolve through Cloudflare/Google instead of the user's
  preferred resolver.

## Failure modes if regressed

- `getActiveDnsServers` returns empty → fall back to public DNS. Works
  but loses Android's policy.
- File write fails (storage permission revoked) → file missing →
  c-ares localhost fallback → tool errors. Mitigation: storage
  permission flow runs before this in `apply_runtime_patches`.
- Network changes mid-session → file stale (still has old WiFi's DNS
  servers). Hasn't bitten in practice; if it does, hook
  `MainEvent::ConfigChanged` to re-populate.

## What this enables downstream

The whole hex-patch flow only works because this bridge keeps the file
real. Without it, hex-patching `/etc/resolv.conf` → `/sdcard/.zed/r`
just relocates the brokenness. With it, every Bun-compiled tool we
install gets DNS that matches Android's actual config — with no proot,
no per-tool wrapper, no syscall ptrace.

## See also

- [hex-patch-resolv-conf.md](https://github.com/Dylanmurzello/zdroid-bootstrap/blob/main/docs/hex-patch-resolv-conf.md)
- [sdcard-dot-zed-namespace.md](sdcard-dot-zed-namespace.md)
