# JNI DNS bridge

**Status:** Active
**Files:** `crates/gpui_android/src/dns_bridge.rs` (`android_dns_servers`, `nativeDnsServersChanged`), `MainActivity.kt` (`getActiveDnsServers`, `watchDnsServers`), `crates/zdroid_runtime/src/install.rs` (`reseed_resolv`)

## Problem

The runtime's programs resolve names with glibc, which reads only
`/etc/resolv.conf`. Android has no such file: it routes DNS through `netd`
and the Java `ConnectivityManager` API, which nothing linked against a
Linux libc can reach. Without the file, glibc falls back to a resolver on
localhost, where nothing listens, so every lookup in the terminal, apt,
git and the agents fails.

## Constraint

A fixed `nameserver 8.8.8.8` would work but:

1. Misses Android's actual DNS config — corporate networks, Pi-hole on
   home Wi-Fi, mobile carrier DNS, a VPN's servers — all bypassed.
2. Goes stale when the user switches Wi-Fi, mobile data or a VPN.
3. Routes traffic away from the user's chosen DNS provider, possibly
   violating their policy.

The file has to mirror **whatever Android is using** at any moment.

## Solution

- **At boot.** `MainActivity.getActiveDnsServers()` returns
  `ConnectivityManager.getLinkProperties(activeNetwork).dnsServers` as a
  comma-joined string; `dns_bridge::android_dns_servers` reads it over JNI,
  and the installer writes the servers into the rootfs's `/etc/resolv.conf`
  on install, and `reseed_resolv` on every boot:

  ```
  nameserver fd4b:cba4:2b5::1
  nameserver 192.168.1.1
  ```

  Public DNS (`1.1.1.1`, `8.8.8.8`) stands in when `ConnectivityManager`
  reports no active network, during early boot before Wi-Fi attaches or
  offline.
- **While running.** `MainActivity.watchDnsServers` registers a
  default-network callback; each `onLinkPropertiesChanged` passes the new
  servers to `NativeBridge.nativeDnsServersChanged`, which calls
  `reseed_resolv` again. Android calls back once right after registering,
  too.
- `reseed_resolv` skips an unchanged file and otherwise renames a new one
  into place, so a lookup running meanwhile reads either the old servers or
  the new ones, never no file.

## Why this works

`ConnectivityManager` is Android's authoritative DNS source, the servers
bionic's resolver would have asked through `netd`. Querying them directly
over UDP takes the same packet path.

## Failure modes if regressed

- `getActiveDnsServers` returns empty → public DNS. Works but ignores
  Android's configuration.
- The callback stops firing → the file keeps the servers from launch;
  after switching networks, names fail to resolve until Zdroid restarts.
- Private DNS (DNS over TLS) isn't used by the runtime's programs: glibc
  sends plain DNS to the network's servers.
