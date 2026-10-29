//! JNI bridge that reads Android's currently active DNS servers.
//!
//! Android has no `/etc/resolv.conf`; it routes DNS through `netd` and
//! the Java `ConnectivityManager` API, which nothing linked against a
//! Linux libc can reach. The rootfs installer seeds the guest's
//! `/etc/resolv.conf` from this list at install time and on every boot.
//!
//! Falls back to public DNS (1.1.1.1, 8.8.8.8) when `ConnectivityManager`
//! reports no active network, which happens during boot before WiFi
//! attaches, or when the user is offline. Afterwards `MainActivity`
//! reports every change of the default network's servers, and the guest's
//! file follows them.

use android_activity::AndroidApp;
use anyhow::{Context, Result};
use jni::JNIEnv;
use jni::objects::{JClass, JString};

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
            log::warn!(
                "dns_bridge: getActiveDnsServers failed: {err:#}; falling back to public DNS"
            );
            fallback_nameservers()
        }
    }
}

fn fallback_nameservers() -> Vec<String> {
    FALLBACK_NAMESERVERS.iter().map(|s| s.to_string()).collect()
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

/// `NativeBridge.nativeDnsServersChanged`, on Android's connectivity thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeDnsServersChanged<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    servers: JString<'local>,
) {
    let servers: String = match env.get_string(&servers) {
        Ok(servers) => servers.into(),
        Err(error) => {
            log::warn!("dns_bridge: decode the DNS servers: {error}");
            return;
        }
    };
    let servers: Vec<String> = servers
        .split(',')
        .filter(|server| !server.is_empty())
        .map(str::to_string)
        .collect();
    if servers.is_empty() {
        return;
    }
    let runtime = zdroid_runtime::Runtime::new(&zdroid_runtime::data_path());
    // The first launch seeds the file when it installs the rootfs.
    if !runtime.is_installed() {
        return;
    }
    match zdroid_runtime::install::reseed_resolv(&runtime, &servers) {
        Ok(()) => log::info!("dns_bridge: resolv.conf follows {servers:?}"),
        Err(error) => log::warn!("dns_bridge: resolv.conf update failed: {error:#}"),
    }
}
