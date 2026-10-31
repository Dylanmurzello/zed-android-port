//! Logs the SELinux domain of the app process at boot.
//!
//! The rootfs engine runs in any untrusted app domain: the kernel only
//! execs the loader in the APK's native library dir, and the Ubuntu
//! binaries are mapped, never exec'd as files. The line is a diagnostic
//! for reading a spawn failure against the domain, not a gate.

pub fn check_selinux_context() {
    let context = std::fs::read_to_string("/proc/self/attr/current").ok();
    log::info!(
        "selinux_canary: /proc/self/attr/current = {:?}",
        context.as_deref().map(str::trim)
    );
}
