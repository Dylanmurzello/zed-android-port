//! Logs the SELinux domain of the app process at boot.
//!
//! The spawn path still relies on `targetSdk = 28`: SELinux puts such
//! apps in `untrusted_app_27`, where `execute_no_trans` on
//! `app_data_file` is allowed. A higher targetSdk lands in a domain where
//! every execve of an app-private binary fails with `EACCES`. Catching
//! that here is faster than bisecting through "why does bash crash".

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
             every app-private binary spawn will EACCES.",
            c.trim()
        );
    }
}
