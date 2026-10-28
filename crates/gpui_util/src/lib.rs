// FluentBuilder
// pub use gpui_util::{FutureExt, Timeout, arc_cow::ArcCow};

use std::{
    env,
    ffi::OsStr,
    ops::AddAssign,
    panic::Location,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
    time::Instant,
};

pub mod arc_cow;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000_u32;

#[cfg(target_os = "windows")]
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    std::process::Command::new(program)
}

/// The program the bridge dispatches through: the rootfs engine loader,
/// by the short name of its stable `<data>/files/bin/zdload` link, which
/// kernel PATH lookup finds because that directory is on the editor's
/// PATH. A short name keeps this lib free of app-package assumptions.
#[cfg(target_os = "android")]
pub const BRIDGE_PROGRAM: &str = "zdload";

/// The app files dir, the host side of the guest view. Set once at boot
/// from `lib.rs`; any absolute-path spawn whose program lives under it
/// is rewritten to route through the loader so it runs in the rootfs.
///
/// `OnceLock` so the slot is initialized exactly once and reads are
/// lock-free for every subsequent `Command::new`.
#[cfg(target_os = "android")]
static ENVIRONMENT_ROOT: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Called from `lib.rs` at boot. Subsequent `Command::new` calls detect
/// absolute paths under `root` and rewrite them to
/// `zdload <abs_path> <args…>`. A second call is a no-op.
#[cfg(target_os = "android")]
pub fn register_environment_root(root: std::path::PathBuf) {
    if ENVIRONMENT_ROOT.set(root).is_err() {
        log::debug!("gpui_util: environment_root already registered; ignoring re-register");
    }
}

/// Android-only env-root bridge helper.
///
/// Zed-the-app runs on Android's bionic libc, but every spawn target
/// that lives under the app files dir (managed Node, downloaded language
/// servers, debug adapters) is a Linux binary meant to run in the
/// rootfs. If the host kernel exec'd such a path directly, the process
/// would fail because the binary's `PT_INTERP`
/// (`/lib/ld-linux-aarch64.so.1`) does not exist on bionic, or because a
/// script's shebang resolves against a non-existent host `/usr/bin/env`.
/// Either way the user sees a bare "No such file or directory" and the
/// language tool silently dies.
///
/// This helper detects the case: program is an absolute path that lives
/// strictly under the registered environment root. When it matches we
/// return `Some(program_path)` and the caller rewrites the spawn to
/// `zdload <program_path> <original_args…>`; the loader maps the binary
/// into the rootfs and runs it there.
///
/// Cost: one read of a `OnceLock` plus a `starts_with` byte-compare on
/// every `Command::new` invocation. Cache-hot, < 1µs.
///
/// Returns `None` when `register_environment_root` was never called,
/// when `program` is a relative path / short name (kernel PATH lookup
/// handles those via the `zd-runtime/<name>` symlinks already), or when
/// the path doesn't live under env_root (system binaries like
/// `/system/bin/sh` keep their native exec semantics).
#[cfg(target_os = "android")]
pub fn env_root_program_path(program: &OsStr) -> Option<std::path::PathBuf> {
    let root = ENVIRONMENT_ROOT.get()?;
    let path = std::path::Path::new(program);
    if !path.is_absolute() {
        return None;
    }
    // `starts_with` matches on full path components, so a literal
    // prefix like `<root>foo` won't false-match against `<root>/foo`.
    if !path.starts_with(root) {
        return None;
    }
    // The farm and `bin/` hold the loader itself. Routing a farm entry
    // through the bridge would make the loader open its own symlink as
    // the guest program; the kernel execs these directly.
    if path.starts_with(root.join("zd-runtime")) || path.starts_with(root.join("bin")) {
        return None;
    }
    Some(path.to_path_buf())
}

/// Translate a `PATH` Zed built for a child, in host terms, into the
/// guest view the loader will apply to that child.
///
/// The loader installs the guest environment from `ZD_GUEST_ENV` at every
/// exec Zed initiates, and its `PATH` entry replaces whatever the spawn
/// carried, so a per-spawn `PATH` never reached the guest: npm's
/// lifecycle scripts run as `sh -c "node install.js"` and need the managed
/// Node's `bin` on it (esbuild installs itself that way). Entries that
/// exist in the guest view keep their place ahead of the configured guest
/// `PATH`; Android's own partitions and the bridge's symlink farm, which
/// points at the loader itself, are host-only and dropped.
///
/// Returns the `ZD_GUEST_ENV` assignment to set on the child, or `None`
/// when this process does not run under the rootfs engine.
#[cfg(target_os = "android")]
pub fn guest_env_for_path(host_path: &OsStr) -> Option<(&'static str, std::ffi::OsString)> {
    let spec = env::var(zd_engine_core::env::GUEST_ENV).ok()?;
    let root = ENVIRONMENT_ROOT.get().map(|root| root.as_path());
    let merged = merge_guest_path(&spec, &host_path.to_string_lossy(), root);
    Some((zd_engine_core::env::GUEST_ENV, merged.into()))
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn merge_guest_path(spec: &str, host_path: &str, root: Option<&std::path::Path>) -> String {
    const HOST_ONLY_PREFIXES: &[&str] = &[
        "/system/",
        "/apex/",
        "/vendor/",
        "/product/",
        "/odm/",
        "/data/app/",
    ];
    let host_only = |entry: &str| {
        HOST_ONLY_PREFIXES
            .iter()
            .any(|prefix| entry.starts_with(prefix))
            || root.is_some_and(|root| {
                let path = std::path::Path::new(entry);
                path.starts_with(root.join("zd-runtime")) || path.starts_with(root.join("bin"))
            })
    };
    let mut entries: Vec<&str> = Vec::new();
    for entry in host_path.split(':') {
        if entry.starts_with('/') && !host_only(entry) && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    let separator = zd_engine_core::LIST_SEPARATOR.to_string();
    let mut items: Vec<String> = Vec::new();
    let mut replaced = false;
    for item in spec.split(&separator) {
        if let Some(guest_path) = item.strip_prefix("PATH=") {
            for entry in guest_path.split(':') {
                if !entry.is_empty() && !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
            items.push(format!("PATH={}", entries.join(":")));
            replaced = true;
        } else if !item.is_empty() {
            items.push(item.to_string());
        }
    }
    if !replaced {
        items.push(format!("PATH={}", entries.join(":")));
    }
    items.join(&separator)
}

#[cfg(test)]
mod guest_path_tests {
    use super::merge_guest_path;
    use std::path::Path;

    const SPEC: &str = "HOME=/root;PATH=/usr/local/bin:/usr/bin:/bin;TMPDIR=/tmp";

    #[test]
    fn managed_node_dir_leads_and_host_dirs_drop() {
        let root = Path::new("/data/user/0/com.zdroid/files");
        let host_path = "/data/user/0/com.zdroid/files/node/v24/bin:/data/user/0/com.zdroid/files/zd-runtime:/data/user/0/com.zdroid/files/bin:/system/bin";
        assert_eq!(
            merge_guest_path(SPEC, host_path, Some(root)),
            "HOME=/root;PATH=/data/user/0/com.zdroid/files/node/v24/bin:/usr/local/bin:/usr/bin:/bin;TMPDIR=/tmp"
        );
    }

    #[test]
    fn a_captured_guest_path_keeps_its_order() {
        let host_path = "/root/.local/bin:/usr/bin:/usr/local/bin:/bin";
        assert_eq!(
            merge_guest_path(SPEC, host_path, None),
            "HOME=/root;PATH=/root/.local/bin:/usr/bin:/usr/local/bin:/bin;TMPDIR=/tmp"
        );
    }

    #[test]
    fn a_spec_without_path_gains_one() {
        assert_eq!(
            merge_guest_path("HOME=/root", "/opt/bin:relative:/system/bin", None),
            "HOME=/root;PATH=/opt/bin"
        );
    }
}

#[cfg(target_os = "android")]
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    // Same env_root bridge as `util::command::Command::new`, mirrored here
    // so synchronous spawn sites get the rewrite too.
    let program = program.as_ref();
    if let Some(program_path) = env_root_program_path(program) {
        let mut cmd = std::process::Command::new(BRIDGE_PROGRAM);
        cmd.arg(program_path);
        return cmd;
    }
    std::process::Command::new(program)
}

#[cfg(target_os = "windows")]
pub fn get_windows_system_shell() -> String {
    use std::path::PathBuf;

    fn find_pwsh_in_programfiles(find_alternate: bool, find_preview: bool) -> Option<PathBuf> {
        #[cfg(target_pointer_width = "64")]
        let env_var = if find_alternate {
            "ProgramFiles(x86)"
        } else {
            "ProgramFiles"
        };

        #[cfg(target_pointer_width = "32")]
        let env_var = if find_alternate {
            "ProgramW6432"
        } else {
            "ProgramFiles"
        };

        let install_base_dir = PathBuf::from(std::env::var_os(env_var)?).join("PowerShell");
        install_base_dir
            .read_dir()
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| matches!(entry.file_type(), Ok(ft) if ft.is_dir()))
            .filter_map(|entry| {
                let dir_name = entry.file_name();
                let dir_name = dir_name.to_string_lossy();

                let version = if find_preview {
                    let dash_index = dir_name.find('-')?;
                    if &dir_name[dash_index + 1..] != "preview" {
                        return None;
                    };
                    dir_name[..dash_index].parse::<u32>().ok()?
                } else {
                    dir_name.parse::<u32>().ok()?
                };

                let exe_path = entry.path().join("pwsh.exe");
                if exe_path.exists() {
                    Some((version, exe_path))
                } else {
                    None
                }
            })
            .max_by_key(|(version, _)| *version)
            .map(|(_, path)| path)
    }

    fn find_pwsh_in_msix(find_preview: bool) -> Option<PathBuf> {
        let msix_app_dir =
            PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Microsoft\\WindowsApps");
        if !msix_app_dir.exists() {
            return None;
        }

        let prefix = if find_preview {
            "Microsoft.PowerShellPreview_"
        } else {
            "Microsoft.PowerShell_"
        };
        msix_app_dir
            .read_dir()
            .ok()?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !matches!(entry.file_type(), Ok(ft) if ft.is_dir()) {
                    return None;
                }

                if !entry.file_name().to_string_lossy().starts_with(prefix) {
                    return None;
                }

                let exe_path = entry.path().join("pwsh.exe");
                exe_path.exists().then_some(exe_path)
            })
            .next()
    }

    fn find_pwsh_in_scoop() -> Option<PathBuf> {
        let pwsh_exe =
            PathBuf::from(std::env::var_os("USERPROFILE")?).join("scoop\\shims\\pwsh.exe");
        pwsh_exe.exists().then_some(pwsh_exe)
    }

    static SYSTEM_SHELL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let locations = [
            || find_pwsh_in_programfiles(false, false),
            || find_pwsh_in_programfiles(true, false),
            || find_pwsh_in_msix(false),
            || find_pwsh_in_programfiles(false, true),
            || find_pwsh_in_msix(true),
            || find_pwsh_in_programfiles(true, true),
            || find_pwsh_in_scoop(),
            || which::which_global("pwsh.exe").ok(),
            || which::which_global("powershell.exe").ok(),
        ];

        locations
            .into_iter()
            .find_map(|f| f())
            .map(|p| p.to_string_lossy().trim().to_owned())
            .inspect(|shell| log::info!("Found powershell in: {}", shell))
            .unwrap_or_else(|| {
                log::warn!("Powershell not found, falling back to `cmd`");
                "cmd.exe".to_string()
            })
    });

    (*SYSTEM_SHELL).clone()
}

pub fn post_inc<T: From<u8> + AddAssign<T> + Copy>(value: &mut T) -> T {
    let prev = *value;
    *value += T::from(1);
    prev
}

pub fn measure<R>(label: &str, f: impl FnOnce() -> R) -> R {
    static ZED_MEASUREMENTS: OnceLock<bool> = OnceLock::new();
    let zed_measurements = ZED_MEASUREMENTS.get_or_init(|| {
        env::var("ZED_MEASUREMENTS")
            .map(|measurements| measurements == "1" || measurements == "true")
            .unwrap_or(false)
    });

    if *zed_measurements {
        let start = Instant::now();
        let result = f();
        let elapsed = start.elapsed();
        eprintln!("{}: {:?}", label, elapsed);
        result
    } else {
        f()
    }
}

#[macro_export]
macro_rules! debug_panic {
    ( $($fmt_arg:tt)* ) => {
        if cfg!(debug_assertions) {
            panic!( $($fmt_arg)* );
        } else {
            let backtrace = std::backtrace::Backtrace::capture();
            log::error!("{}\n{:?}", format_args!($($fmt_arg)*), backtrace);
        }
    };
}

#[track_caller]
pub fn some_or_debug_panic<T>(option: Option<T>) -> Option<T> {
    #[cfg(debug_assertions)]
    if option.is_none() {
        panic!("Unexpected None");
    }
    option
}

/// Expands to an immediately-invoked function expression. Good for using the ? operator
/// in functions which do not return an Option or Result.
///
/// Accepts a normal block, an async block, or an async move block.
#[macro_export]
macro_rules! maybe {
    ($block:block) => {
        (|| $block)()
    };
    (async $block:block) => {
        (async || $block)()
    };
    (async move $block:block) => {
        (async move || $block)()
    };
}
pub trait ResultExt<E> {
    type Ok;

    fn log_err(self) -> Option<Self::Ok>;
    /// Like [`ResultExt::log_err`], but uses `{:?}` formatting so `anyhow::Error` values emit their
    /// full backtrace. Reach for this only when a backtrace is genuinely wanted — most call sites
    /// should stick with `log_err` / `warn_on_err`, whose output is a single chained error message.
    fn log_err_with_backtrace(self) -> Option<Self::Ok>
    where
        E: std::fmt::Debug;
    /// Assert that this result should never be an error in development or tests.
    fn debug_assert_ok(self, reason: &str) -> Self;
    fn warn_on_err(self) -> Option<Self::Ok>;
    fn log_with_level(self, level: log::Level) -> Option<Self::Ok>;
    fn anyhow(self) -> anyhow::Result<Self::Ok>
    where
        E: Into<anyhow::Error>;
}

impl<T, E> ResultExt<E> for Result<T, E>
where
    E: std::fmt::Display,
{
    type Ok = T;

    #[track_caller]
    fn log_err(self) -> Option<T> {
        self.log_with_level(log::Level::Error)
    }

    #[track_caller]
    fn log_err_with_backtrace(self) -> Option<T>
    where
        E: std::fmt::Debug,
    {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                log_error_with_caller(
                    *Location::caller(),
                    DebugAsDisplay(&error),
                    log::Level::Error,
                );
                None
            }
        }
    }

    #[track_caller]
    fn debug_assert_ok(self, reason: &str) -> Self {
        if let Err(error) = &self {
            debug_panic!("{reason} - {error:#}");
        }
        self
    }

    #[track_caller]
    fn warn_on_err(self) -> Option<T> {
        self.log_with_level(log::Level::Warn)
    }

    #[track_caller]
    fn log_with_level(self, level: log::Level) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                log_error_with_caller(*Location::caller(), error, level);
                None
            }
        }
    }

    fn anyhow(self) -> anyhow::Result<T>
    where
        E: Into<anyhow::Error>,
    {
        self.map_err(Into::into)
    }
}

fn log_error_with_caller<E>(caller: core::panic::Location<'_>, error: E, level: log::Level)
where
    E: std::fmt::Display,
{
    #[cfg(not(windows))]
    let file = caller.file();
    #[cfg(windows)]
    let file = caller.file().replace('\\', "/");
    // In this codebase all crates reside in a `crates` directory,
    // so discard the prefix up to that segment to find the crate name
    let file = file.split_once("crates/");
    let target = file.as_ref().and_then(|(_, s)| s.split_once("/src/"));

    let module_path = target.map(|(krate, module)| {
        if module.starts_with(krate) {
            module.trim_end_matches(".rs").replace('/', "::")
        } else {
            krate.to_owned() + "::" + &module.trim_end_matches(".rs").replace('/', "::")
        }
    });
    let file = file.map(|(_, file)| format!("crates/{file}"));
    log::logger().log(
        &log::Record::builder()
            .target(module_path.as_deref().unwrap_or(""))
            .module_path(file.as_deref())
            .args(format_args!("{:#}", error))
            .file(Some(caller.file()))
            .line(Some(caller.line()))
            .level(level)
            .build(),
    );
}

pub fn log_err<E: std::fmt::Display>(error: &E) {
    log_error_with_caller(*Location::caller(), error, log::Level::Error);
}

// Forces `{:?}` formatting through a `Display`-bounded logging helper so `anyhow::Error` emits a
// backtrace instead of the single-line chained message produced by its `Display`/`{:#}` forms.
struct DebugAsDisplay<'a, E>(&'a E);

impl<E: std::fmt::Debug> std::fmt::Display for DebugAsDisplay<'_, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

pub trait TryFutureExt {
    fn log_err(self) -> LogErrorFuture<Self>
    where
        Self: Sized;

    fn log_tracked_err(self, location: core::panic::Location<'static>) -> LogErrorFuture<Self>
    where
        Self: Sized;

    fn warn_on_err(self) -> LogErrorFuture<Self>
    where
        Self: Sized;
    fn unwrap(self) -> UnwrapFuture<Self>
    where
        Self: Sized;
}

/// `{:?}`-formatting companion to [`TryFutureExt`]; emits a backtrace for `anyhow::Error`. Prefer
/// [`TryFutureExt`] unless a backtrace is genuinely wanted.
pub trait TryFutureExtBacktrace {
    fn log_err_with_backtrace(self) -> LogErrorWithBacktraceFuture<Self>
    where
        Self: Sized;

    fn log_tracked_err_with_backtrace(
        self,
        location: core::panic::Location<'static>,
    ) -> LogErrorWithBacktraceFuture<Self>
    where
        Self: Sized;
}

impl<F, T, E> TryFutureExt for F
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    #[track_caller]
    fn log_err(self) -> LogErrorFuture<Self>
    where
        Self: Sized,
    {
        let location = Location::caller();
        LogErrorFuture(self, log::Level::Error, *location)
    }

    fn log_tracked_err(self, location: core::panic::Location<'static>) -> LogErrorFuture<Self>
    where
        Self: Sized,
    {
        LogErrorFuture(self, log::Level::Error, location)
    }

    #[track_caller]
    fn warn_on_err(self) -> LogErrorFuture<Self>
    where
        Self: Sized,
    {
        let location = Location::caller();
        LogErrorFuture(self, log::Level::Warn, *location)
    }

    fn unwrap(self) -> UnwrapFuture<Self>
    where
        Self: Sized,
    {
        UnwrapFuture(self)
    }
}

impl<F, T, E> TryFutureExtBacktrace for F
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Debug,
{
    #[track_caller]
    fn log_err_with_backtrace(self) -> LogErrorWithBacktraceFuture<Self>
    where
        Self: Sized,
    {
        let location = Location::caller();
        LogErrorWithBacktraceFuture(self, log::Level::Error, *location)
    }

    fn log_tracked_err_with_backtrace(
        self,
        location: core::panic::Location<'static>,
    ) -> LogErrorWithBacktraceFuture<Self>
    where
        Self: Sized,
    {
        LogErrorWithBacktraceFuture(self, log::Level::Error, location)
    }
}

#[must_use]
pub struct LogErrorFuture<F>(F, log::Level, core::panic::Location<'static>);

impl<F, T, E> Future for LogErrorFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let level = self.1;
        let location = self.2;
        let inner = unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().0) };
        match inner.poll(cx) {
            Poll::Ready(output) => Poll::Ready(match output {
                Ok(output) => Some(output),
                Err(error) => {
                    log_error_with_caller(location, error, level);
                    None
                }
            }),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[must_use]
pub struct LogErrorWithBacktraceFuture<F>(F, log::Level, core::panic::Location<'static>);

impl<F, T, E> Future for LogErrorWithBacktraceFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Debug,
{
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let level = self.1;
        let location = self.2;
        let inner = unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().0) };
        match inner.poll(cx) {
            Poll::Ready(output) => Poll::Ready(match output {
                Ok(output) => Some(output),
                Err(error) => {
                    log_error_with_caller(location, DebugAsDisplay(&error), level);
                    None
                }
            }),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct UnwrapFuture<F>(F);

impl<F, T, E> Future for UnwrapFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Debug,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let inner = unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().0) };
        match inner.poll(cx) {
            Poll::Ready(result) => Poll::Ready(result.unwrap()),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct Deferred<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Deferred<F> {
    /// Drop without running the deferred function.
    pub fn abort(mut self) {
        self.0.take();
    }
}

impl<F: FnOnce()> Drop for Deferred<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f()
        }
    }
}

/// Run the given function when the returned value is dropped (unless it's cancelled).
#[must_use]
pub fn defer<F: FnOnce()>(f: F) -> Deferred<F> {
    Deferred(Some(f))
}

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TypeIdHashBuilder;

impl std::hash::BuildHasher for TypeIdHashBuilder {
    type Hasher = TypeIdHasher;

    fn build_hasher(&self) -> Self::Hasher {
        TypeIdHasher::default()
    }
}

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TypeIdHasher {
    value: u64,
}

impl std::hash::Hasher for TypeIdHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // TypeId should only hash its first 8 bytes
        if let Some(bytes) = bytes.get(..8) {
            bytes
                .as_array()
                .map(|&array| self.value = u64::from_ne_bytes(array))
                .unwrap_or_else(|| unreachable!("slice was sliced to 8 bytes"));
        } else {
            debug_panic!(
                "expected a 64-bit value, did you use this hasher with something other than a TypeId?"
            );
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.value
    }
}

#[test]
fn type_id_hasher() {
    use core::any::TypeId;
    use core::hash::{Hash, Hasher};
    fn verify_hashing_with(type_id: TypeId) {
        let mut hasher = TypeIdHasher::default();
        type_id.hash(&mut hasher);
        assert_ne!(hasher.finish(), 0);
    }
    // Pick a variety of types, just to demonstrate it’s all sane. Normal, zero-sized, unsized, &c.
    verify_hashing_with(TypeId::of::<usize>());
    verify_hashing_with(TypeId::of::<()>());
    verify_hashing_with(TypeId::of::<str>());
    verify_hashing_with(TypeId::of::<&str>());
    verify_hashing_with(TypeId::of::<Vec<u8>>());
}

pub fn truncate_to_bottom_n_sorted_by<T, F>(items: &mut Vec<T>, limit: usize, compare: &F)
where
    F: Fn(&T, &T) -> std::cmp::Ordering,
{
    if limit == 0 {
        items.truncate(0);
    }
    if items.len() <= limit {
        items.sort_by(compare);
        return;
    }
    // When limit is near to items.len() it may be more efficient to sort the whole list and
    // truncate, rather than always doing selection first as is done below. It's hard to analyze
    // where the threshold for this should be since the quickselect style algorithm used by
    // `select_nth_unstable_by` makes the prefix partially sorted, and so its work is not wasted -
    // the expected number of comparisons needed by `sort_by` is less than it is for some arbitrary
    // unsorted input.
    items.select_nth_unstable_by(limit, compare);
    items.truncate(limit);
    items.sort_by(compare);
}
