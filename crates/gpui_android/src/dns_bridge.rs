//! JNI bridge that reads Android's currently active DNS servers.
//!
//! Android has no `/etc/resolv.conf`; it routes DNS through `netd` and
//! the Java `ConnectivityManager` API, which nothing linked against a
//! Linux libc can reach. The rootfs installer seeds the guest's
//! `/etc/resolv.conf` from this list at install time and on every boot.
//!
//! Falls back to public DNS (1.1.1.1, 8.8.8.8) when `ConnectivityManager`
//! reports no active network, which happens during boot before WiFi
//! attaches, or when the user is offline.

use android_activity::AndroidApp;
use anyhow::{Context, Result};
use jni::{JavaVM, objects::JObject, objects::JString};

const FALLBACK_NAMESERVERS: &[&str] = &["1.1.1.1", "8.8.8.8"];

/// Active-network DNS servers, or the public fallback when Android
/// reports none. Never empty.
pub fn android_dns_servers(android_app: &AndroidApp) -> Vec<String> {
    match query_android_dns(android_app) {
        Ok(servers) if !servers.is_empty() => servers,
        Ok(_) => {
            log::info!(
                "dns_bridge: ConnectivityManager returned no DNS servers \
                 (no active network?); falling back to public DNS"
            );
            fallback_nameservers()
        }
        Err(err) => {
            log::warn!("dns_bridge: getActiveDnsServers failed: {err:#}; falling back to public DNS");
            fallback_nameservers()
        }
    }
}

fn fallback_nameservers() -> Vec<String> {
    FALLBACK_NAMESERVERS.iter().map(|s| s.to_string()).collect()
}

fn query_android_dns(android_app: &AndroidApp) -> Result<Vec<String>> {
    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread for dns query")?;
    let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };
    let result = env
        .call_method(&activity, "getActiveDnsServers", "()Ljava/lang/String;", &[])
        .context("MainActivity.getActiveDnsServers")?;
    let result_obj = result.l().context("getActiveDnsServers returned non-object")?;
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
}
