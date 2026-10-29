//! Pointer authentication on the app's threads.
//!
//! bionic resets the return-address key (IA) on every thread it creates
//! for apps targeting API 31 and later (`pthread_create`, bionic commit
//! dcbacd676: the reset moved from the zygote fork to thread creation,
//! and is skipped for older targets because Chrome's sandbox rejects the
//! prctl). A return address signed on one thread therefore fails to
//! authenticate on another. wasmtime's fiber switch signs the address it
//! resumes a fiber at on the thread that created the fiber and checks it
//! on the thread that resumes it, and Zed's extension host runs those
//! futures on a multi-threaded runtime, so at targetSdk 35 the first wasm
//! call resumed on another worker died with a PAC exception (SIGILL in
//! `wasmtime_fiber_switch`). The targetSdk 28 builds never saw it: one
//! key for the whole process.
//!
//! The fix disables the IA key for this thread before any other thread
//! exists; the enabled-key mask is inherited by every thread created
//! afterwards, so `paciasp`, `autiasp` and `pacia1716` become no-ops on
//! all of them, the state a pre-31 target had. The other keys stay on.

#[cfg(target_arch = "aarch64")]
const PR_PAC_SET_ENABLED_KEYS: libc::c_int = 60;
#[cfg(target_arch = "aarch64")]
const PR_PAC_GET_ENABLED_KEYS: libc::c_int = 61;
#[cfg(target_arch = "aarch64")]
const PR_PAC_APIAKEY: libc::c_ulong = 1;

/// Turn the IA key off for the calling thread and its future threads.
/// Call before the first `std::thread::spawn`.
#[cfg(target_arch = "aarch64")]
pub fn disable_return_address_key() {
    // SAFETY: a prctl with integer arguments only; kernels before 5.13
    // answer EINVAL, which is logged and harmless (they also predate the
    // per-thread reset).
    let result = unsafe { libc::prctl(PR_PAC_SET_ENABLED_KEYS, PR_PAC_APIAKEY, 0, 0, 0) };
    if result == 0 {
        log::info!("pointer_authentication: IA key disabled for this process's threads");
    } else {
        log::warn!(
            "pointer_authentication: PR_PAC_SET_ENABLED_KEYS failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(target_arch = "aarch64"))]
pub fn disable_return_address_key() {}

/// Logs which PAC keys are enabled on this thread and on a fresh thread,
/// and whether both sign a pointer to the same value: the canary for the
/// per-thread reset described above.
#[cfg(target_arch = "aarch64")]
pub fn check_pointer_authentication() {
    fn enabled_keys() -> libc::c_int {
        // SAFETY: a query prctl with no pointers; -1 on kernels without it.
        unsafe { libc::prctl(PR_PAC_GET_ENABLED_KEYS, 0, 0, 0, 0) }
    }
    fn sign(value: u64, modifier: u64) -> u64 {
        let signed: u64;
        // SAFETY: `pacia1716` (hint #8) signs x17 with the modifier in x16
        // and is a no-op on cores without pointer authentication; the two
        // registers are declared clobbered.
        unsafe {
            core::arch::asm!(
                "mov x17, {value}",
                "mov x16, {modifier}",
                "hint #8",
                "mov {signed}, x17",
                value = in(reg) value,
                modifier = in(reg) modifier,
                signed = out(reg) signed,
                out("x16") _,
                out("x17") _,
            );
        }
        signed
    }
    const PROBE: u64 = 0x0000_7f12_3456_7890;
    const MODIFIER: u64 = 0x1234;
    let main_keys = enabled_keys();
    let main_signature = sign(PROBE, MODIFIER);
    let other = std::thread::spawn(|| (enabled_keys(), sign(PROBE, MODIFIER))).join();
    match other {
        Ok((thread_keys, thread_signature)) => log::info!(
            "pac: enabled keys main={main_keys:#x} thread={thread_keys:#x}; signing {} ({})",
            if main_signature == thread_signature {
                "agrees across threads"
            } else {
                "DIFFERS across threads"
            },
            if main_signature == PROBE {
                "no-op on this core or key"
            } else {
                "active"
            },
        ),
        Err(_) => log::warn!("pac: probe thread panicked"),
    }
}

#[cfg(not(target_arch = "aarch64"))]
pub fn check_pointer_authentication() {}
