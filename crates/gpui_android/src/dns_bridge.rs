//! JNI bridge that pulls Android's currently-active DNS server IPs and
//! materializes them as a `resolv.conf`-format file at `/sdcard/.zed/r`.
//!
//! Why this exists: Bun-compiled CLIs (claude, codex, future tools) link
//! musl statically and call c-ares with the compile-time-hardcoded path
//! `/etc/resolv.conf`. Android has no `/etc/resolv.conf` (it routes DNS
//! through `netd` and the Java `ConnectivityManager` API, which musl
//! can't reach). The launcher generator's deep-walk hex-patches the
//! literal `/etc/resolv.conf` in those binaries to the shorter
//! `/sdcard/.zed/r` (16 → 14 bytes, padded with NULs to keep the slot
//! width). This module's job is to make sure that path exists with
//! valid `nameserver <IP>` lines whenever the app boots, so the
//! patched binaries find real DNS servers when c-ares opens it.
//!
//! The hidden `.zed/` namespace keeps the writes off the user-visible
//! `/sdcard` root. App-private storage would be cleaner but its path
//! (`/data/data/<pkg>/files/...`) is way longer than the 16-byte slot
//! the binary patch can fit.
//!
//! Falls back to public DNS (1.1.1.1, 8.8.8.8) if `ConnectivityManager`
//! gives us no active network — happens during boot before WiFi
//! attaches, or when the user is offline. The patched CLIs may still
//! fail to resolve at that point, but they'll fail with a clean
//! `network unreachable` instead of a `/etc/resolv.conf: no such file`.

use std::path::Path;

use android_activity::AndroidApp;
use anyhow::{Context, Result};
use jni::objects::JString;

const RESOLV_CONF_PATH: &str = "/sdcard/.zed/r";
const FALLBACK_NAMESERVERS: &[&str] = &["1.1.1.1", "8.8.8.8"];

/// Read Android's active-network DNS servers via JNI, write them to
/// `/sdcard/.zed/r` in resolv.conf format. Idempotent — overwrites the
/// file on every call so a network change followed by a re-call picks
/// up the new servers. Caller fires this at boot and may fire again on
/// `MainEvent::ConfigChanged` if the network appears to have switched.
pub fn populate_resolv_conf(android_app: &AndroidApp) {
    match populate_inner(android_app) {
        Ok(servers) => log::info!(
            "dns_bridge: wrote {} ({} nameserver{} from Android)",
            RESOLV_CONF_PATH,
            servers,
            if servers == 1 { "" } else { "s" }
        ),
        Err(err) => log::warn!("dns_bridge: populate_resolv_conf failed: {err:#}"),
    }
}

fn populate_inner(android_app: &AndroidApp) -> Result<usize> {
    let servers = query_android_dns(android_app).unwrap_or_default();
    let nameservers: Vec<String> = if servers.is_empty() {
        log::info!(
            "dns_bridge: ConnectivityManager returned no DNS servers \
             (no active network?); falling back to public DNS"
        );
        FALLBACK_NAMESERVERS.iter().map(|s| s.to_string()).collect()
    } else {
        servers
    };

    let path = Path::new(RESOLV_CONF_PATH);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }

    let mut content = String::new();
    for ns in &nameservers {
        content.push_str("nameserver ");
        content.push_str(ns);
        content.push('\n');
    }
    std::fs::write(path, content.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(nameservers.len())
}

fn query_android_dns(android_app: &AndroidApp) -> Result<Vec<String>> {
    crate::jni_env::with_activity(android_app, |env, activity| {
        let result = env
            .call_method(activity, "getActiveDnsServers", "()Ljava/lang/String;", &[])
            .context("MainActivity.getActiveDnsServers")?;
        let result_obj = result
            .l()
            .context("getActiveDnsServers returned non-object")?;
        if result_obj.is_null() {
            return Ok(Vec::new());
        }
        let jstr: JString = result_obj.into();
        let csv: String = env
            .get_string(&jstr)
            .context("decode getActiveDnsServers result")?
            .into();
        Ok(csv
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    })
}
