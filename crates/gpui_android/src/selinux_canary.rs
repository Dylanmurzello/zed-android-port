//! Checks the SELinux domain the app runs in, which decides whether it can
//! run programs from its own storage. See `docs/workarounds/selinux-canary.md`.

/// Logs the process's SELinux context. If `targetSdk >= 29` ever sneaks
/// back into `build.gradle.kts`, the JVM lands in `untrusted_app_all`
/// where `execute_no_trans` on `app_data_file` is denied — every spawned
/// binary fails with `EACCES`. Catching it loudly here is faster than
/// bisecting through "why does bash crash".
pub fn check_selinux_context() {
    let context = std::fs::read_to_string("/proc/self/attr/current").ok();
    log::info!("selinux_canary: /proc/self/attr/current = {:?}", context);
    let Some(c) = context.as_deref() else {
        return;
    };
    if !c.contains("untrusted_app_27") && !c.contains("untrusted_app_25") {
        log::error!(
            "selinux_canary: SELinux domain {} disallows execute_no_trans on \
             app_data_file. Verify build.gradle.kts pins targetSdk=28; otherwise \
             every $PREFIX/bin/* spawn will EACCES.",
            c.trim()
        );
    }
}
