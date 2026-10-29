//! Keep the editor's children alive while the app is in the background,
//! and leave none of them behind when it quits.
//!
//! Every tool Zed runs (language servers, shells, debug adapters, git) is a
//! child of the app process, and Android treats an app's children as
//! "phantom processes": once the app is cached (not visible, no foreground
//! service) the phantom process killer stops them and the low-memory
//! killer takes the app itself early. The chroot adapter never saw this
//! because the root daemon forked its children; the rootfs engine's
//! children are ours. A foreground service holds the process out of the
//! cached state for as long as children exist.
//!
//! Android mounts `/proc` with `hidepid=invisible`, so an app sees only
//! the processes of its own uid: itself and its descendants. That makes
//! the kernel's process table the registry of children, complete across
//! every spawn path (the command bridge, PTY shells, tools the children
//! start themselves) and cheap to read, a few entries per scan.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use anyhow::{Context as _, Result};
use jni::{JNIEnv, JavaVM, objects::JClass, objects::JObject};

/// How often the process table is read while the app runs.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// How long the service outlives the last child, so a language server
/// restart or a new terminal does not bounce the notification.
const STOP_DEBOUNCE: Duration = Duration::from_secs(30);
/// Time given to SIGTERM before SIGKILL at quit.
const TERMINATE_GRACE: Duration = Duration::from_millis(200);
const TERMINATE_POLL: Duration = Duration::from_millis(20);

static KEEP_AWAKE: AtomicBool = AtomicBool::new(false);

/// Mirror of `android_runtime.keep_awake`: hold a partial wake lock for
/// the service's lifetime so builds and indexing continue with the screen
/// off. Pushed from the app's settings observer.
pub fn set_keep_awake(enabled: bool) {
    KEEP_AWAKE.store(enabled, Ordering::Release);
}

struct ServiceState {
    last_scan: Option<Instant>,
    idle_since: Option<Instant>,
    /// `(active, keep_awake)` as last pushed to the Activity; `None` until
    /// the first push.
    pushed: Option<(bool, bool)>,
    /// No push before this instant after a failed one: Android 12+ refuses
    /// a foreground service start from the background, and retrying every
    /// scan would only repeat the exception.
    retry_after: Option<Instant>,
}

static SERVICE: Mutex<ServiceState> = Mutex::new(ServiceState {
    last_scan: None,
    idle_since: None,
    pushed: None,
    retry_after: None,
});

const RETRY_BACKOFF: Duration = Duration::from_secs(30);

fn service_state() -> std::sync::MutexGuard<'static, ServiceState> {
    match SERVICE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Reconcile the foreground service with the process tree. Called from the
/// platform loop every tick; reads `/proc` every [`SCAN_INTERVAL`] and
/// calls into the Activity only on a transition.
pub(crate) fn tick(android_app: &AndroidApp) {
    let now = Instant::now();
    let keep_awake = KEEP_AWAKE.load(Ordering::Acquire);
    let mut state = service_state();
    let due = state
        .last_scan
        .is_none_or(|last| now.duration_since(last) >= SCAN_INTERVAL);
    let was_active = state.pushed.is_some_and(|(active, _)| active);
    let active = if due {
        state.last_scan = Some(now);
        if descendants().iter().any(|process| process.is_live()) {
            state.idle_since = None;
            true
        } else {
            let idle_since = *state.idle_since.get_or_insert(now);
            was_active && now.duration_since(idle_since) < STOP_DEBOUNCE
        }
    } else {
        was_active
    };
    let wanted = (active, keep_awake);
    if state.pushed == Some(wanted) || (!active && state.pushed.is_none()) {
        return;
    }
    if state.retry_after.is_some_and(|instant| now < instant) {
        return;
    }
    match set_runtime_service(android_app, active, keep_awake) {
        Ok(()) => {
            log::info!("runtime_service: active={active} keep_awake={keep_awake}");
            state.pushed = Some(wanted);
            state.retry_after = None;
        }
        Err(error) => {
            log::warn!("runtime_service: set active={active}: {error:#}");
            state.retry_after = Some(now + RETRY_BACKOFF);
        }
    }
}

/// Quit path: stop the service and end every descendant before the process
/// exits, since `std::process::exit` runs no destructors and the kernel
/// reparents orphans to init, where an idle language server would live on.
pub(crate) fn shutdown(android_app: &AndroidApp) {
    let mut state = service_state();
    if state.pushed.is_some_and(|(active, _)| active)
        && let Err(error) = set_runtime_service(android_app, false, false)
    {
        log::warn!("runtime_service: stop at quit: {error:#}");
    }
    state.pushed = Some((false, false));
    drop(state);
    log::info!("runtime_service: quit, service stopped");
    terminate_descendants();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Process {
    pid: i32,
    parent: i32,
    state: u8,
}

impl Process {
    /// A zombie has exited and only waits for its parent's `wait`; it
    /// holds no resources and cannot be signalled.
    fn is_live(&self) -> bool {
        !matches!(self.state, b'Z' | b'X')
    }
}

/// Every visible process other than this one. Under `hidepid` that is the
/// app's own uid; the ancestry check below keeps a stray same-uid process
/// (an `adb shell run-as` session, an orphan of an earlier instance) out.
fn process_table() -> Vec<Process> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let own = std::process::id() as i32;
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid != own)
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            parse_stat(pid, &stat)
        })
        .collect()
}

/// `pid (comm) state ppid pgid ...`; `comm` may contain spaces and
/// parentheses, so the fields are taken after the last `)`.
fn parse_stat(pid: i32, stat: &str) -> Option<Process> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace();
    let state = *fields.next()?.as_bytes().first()?;
    let parent = fields.next()?.parse().ok()?;
    Some(Process { pid, parent, state })
}

/// Transitive children of this process.
fn descendants() -> Vec<Process> {
    let table = process_table();
    let mut roots = vec![std::process::id() as i32];
    let mut found: Vec<Process> = Vec::new();
    while let Some(root) = roots.pop() {
        for process in &table {
            if process.parent == root && !found.contains(process) {
                found.push(*process);
                roots.push(process.pid);
            }
        }
    }
    found
}

/// SIGTERM every live descendant, give them [`TERMINATE_GRACE`], then
/// SIGKILL whatever is left. Signals go to each process rather than to a
/// group: the app itself sits in zygote's process group, and a terminal's
/// shell owns a session of its own.
pub fn terminate_descendants() {
    let started = Instant::now();
    let live: Vec<Process> = descendants().into_iter().filter(Process::is_live).collect();
    if live.is_empty() {
        log::info!("runtime_service: no descendants to terminate");
        return;
    }
    log::info!("runtime_service: terminating {} descendant(s)", live.len());
    for process in &live {
        signal(process.pid, libc::SIGTERM);
    }
    while started.elapsed() < TERMINATE_GRACE {
        std::thread::sleep(TERMINATE_POLL);
        if !descendants().iter().any(Process::is_live) {
            return;
        }
    }
    let stubborn: Vec<Process> = descendants().into_iter().filter(Process::is_live).collect();
    for process in &stubborn {
        signal(process.pid, libc::SIGKILL);
    }
    log::info!(
        "runtime_service: {} descendant(s) killed after the grace period",
        stubborn.len()
    );
}

fn signal(pid: i32, signal: i32) {
    // SAFETY: a plain kill(2) on a pid read from /proc; ESRCH for a process
    // that exited in between is the expected race and is ignored.
    unsafe {
        libc::kill(pid, signal);
    }
}

/// `MainActivity.setRuntimeServiceActive(active, keepAwake)`.
fn set_runtime_service(android_app: &AndroidApp, active: bool, keep_awake: bool) -> Result<()> {
    let (vm, activity) = activity(android_app)?;
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread")?;
    env.call_method(
        &activity,
        "setRuntimeServiceActive",
        "(ZZ)V",
        &[active.into(), keep_awake.into()],
    )
    .context("call MainActivity.setRuntimeServiceActive")?;
    Ok(())
}

/// Whether the user already exempted the app from battery optimization.
pub fn ignoring_battery_optimizations() -> Result<bool> {
    let android_app = crate::updater::android_app()?;
    let (vm, activity) = activity(android_app)?;
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread")?;
    let ignoring = env
        .call_method(&activity, "isIgnoringBatteryOptimizations", "()Z", &[])
        .context("call MainActivity.isIgnoringBatteryOptimizations")?
        .z()?;
    Ok(ignoring)
}

/// Open the system dialog that exempts the app from battery optimization.
/// Returns `true` when the dialog opened or the exemption already holds.
pub fn request_battery_exemption() -> Result<bool> {
    let android_app = crate::updater::android_app()?;
    let (vm, activity) = activity(android_app)?;
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread")?;
    let opened = env
        .call_method(&activity, "requestIgnoreBatteryOptimizations", "()Z", &[])
        .context("call MainActivity.requestIgnoreBatteryOptimizations")?
        .z()?;
    Ok(opened)
}

fn activity(android_app: &AndroidApp) -> Result<(JavaVM, JObject<'static>)> {
    let vm_ptr = android_app.vm_as_ptr();
    let activity_ptr = android_app.activity_as_ptr();
    anyhow::ensure!(
        !vm_ptr.is_null() && !activity_ptr.is_null(),
        "AndroidApp vm/activity pointer is null"
    );
    // SAFETY: both pointers come from android-activity and stay valid for
    // the Activity's lifetime; the JObject is a borrowed reference that is
    // never deleted here.
    let vm = unsafe { JavaVM::from_raw(vm_ptr as _) }.context("JavaVM::from_raw")?;
    let activity = unsafe { JObject::from_raw(activity_ptr as _) };
    Ok((vm, activity))
}

/// `MainActivity.onDestroy` calls this before it kills the process, so a
/// task swiped away in Recents leaves no language server behind.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeBeforeProcessExit<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) {
    terminate_descendants();
}

#[cfg(test)]
mod tests {
    use super::{Process, parse_stat};

    #[test]
    fn stat_fields_follow_the_last_parenthesis() {
        let stat = "18659 (bash) S 14532 18659 14532 34816 18659 4194560 0\n";
        assert_eq!(
            parse_stat(18659, stat),
            Some(Process {
                pid: 18659,
                parent: 14532,
                state: b'S'
            })
        );
        let odd = "7 (a (b) c) Z 1 7 7 0 -1\n";
        assert_eq!(
            parse_stat(7, odd),
            Some(Process {
                pid: 7,
                parent: 1,
                state: b'Z'
            })
        );
        assert!(!parse_stat(7, odd).is_some_and(|process| process.is_live()));
    }
}
