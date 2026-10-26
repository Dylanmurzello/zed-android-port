//! Engine configuration copied out of the environment at startup into
//! `.bss`, so the trap handler and the exec path read fixed storage and
//! never the guest's (mutable) environment block.

use core::cell::UnsafeCell;
use core::fmt;

use zd_engine_core::bindings::{Bindings, BindingsError};
use zd_engine_core::path::{MAX_PATH, is_absolute};
use zd_engine_core::stack::{env_key, env_value};
use zd_engine_core::translate::Translator;
use zd_engine_core::{LIST_SEPARATOR, env};

use crate::sys;

/// A `static` the loader writes once during single-threaded startup and
/// reads from the trap handler afterwards.
pub struct Global<T>(UnsafeCell<T>);

// SAFETY: writes happen only in `init`, before the guest (and so any
// second thread) exists; every later access is a read.
unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    pub fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// NUL-terminated byte string with fixed storage.
pub struct Buf {
    data: [u8; MAX_PATH],
    len: usize,
}

impl Buf {
    pub const fn new() -> Self {
        Self {
            data: [0; MAX_PATH],
            len: 0,
        }
    }

    pub fn set(&mut self, bytes: &[u8]) -> Result<(), InitError> {
        if bytes.len() >= MAX_PATH {
            return Err(InitError::TooLong);
        }
        self.data[..bytes.len()].copy_from_slice(bytes);
        self.data[bytes.len()] = 0;
        self.len = bytes.len();
        Ok(())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// `KEY=VALUE` entries applied to the guest environment only, stored
/// NUL-separated so each one is directly usable as an `envp` string.
pub struct GuestEnv {
    data: [u8; MAX_PATH],
    len: usize,
}

impl GuestEnv {
    pub const fn new() -> Self {
        Self {
            data: [0; MAX_PATH],
            len: 0,
        }
    }

    fn add(&mut self, entry: &[u8]) -> Result<(), InitError> {
        if entry.is_empty() || !entry.contains(&b'=') {
            return Err(InitError::BadGuestEnv);
        }
        if self.len + entry.len() + 1 > self.data.len() {
            return Err(InitError::TooLong);
        }
        self.data[self.len..self.len + entry.len()].copy_from_slice(entry);
        self.len += entry.len();
        self.data[self.len] = 0;
        self.len += 1;
        Ok(())
    }

    /// Each entry, NUL-terminated in place.
    pub fn entries(&self) -> impl Iterator<Item = &[u8]> {
        self.data[..self.len]
            .split(|&byte| byte == 0)
            .filter(|entry| !entry.is_empty())
    }

    pub fn overrides(&self, entry: &[u8]) -> bool {
        let key = env_key(entry);
        self.entries().any(|candidate| env_key(candidate) == key)
    }
}

/// Whether an inherited entry stays out of the guest's environment:
/// overridden by the guest env, or describing this exec only.
pub fn hidden_from_guest(entry: &[u8]) -> bool {
    let key = env_key(entry);
    env::PER_EXEC.iter().any(|name| name.as_bytes() == key) || get().guest_env.overrides(entry)
}

pub struct Config {
    pub rootfs: Buf,
    pub binds: Buf,
    pub log: Buf,
    /// Guest program named by `ZD_PROGRAM`, when exec'd with the guest's
    /// own argv.
    pub program: Buf,
    pub selftest: bool,
    /// The guest's auxiliary vector as built for it, served as
    /// `/proc/self/auxv`.
    pub auxv_ptr: usize,
    pub auxv_len: usize,
    /// Guest-visible path of the executable actually loaded, for
    /// `/proc/self/exe`.
    pub guest_exe: Buf,
    /// Host path of this loader, from `/proc/self/exe` before any guest
    /// code runs.
    pub loader_path: Buf,
    pub guest_env: GuestEnv,
    pub fake_root: bool,
    pub page_size: usize,
    pub translator: Option<Translator<'static>>,
}

impl Config {
    pub const fn new() -> Self {
        Self {
            rootfs: Buf::new(),
            binds: Buf::new(),
            log: Buf::new(),
            program: Buf::new(),
            selftest: false,
            auxv_ptr: 0,
            auxv_len: 0,
            guest_exe: Buf::new(),
            loader_path: Buf::new(),
            guest_env: GuestEnv::new(),
            fake_root: false,
            page_size: 0,
            translator: None,
        }
    }
}

static CONFIG: Global<Config> = Global::new(Config::new());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    MissingRootfs,
    RootfsNotAbsolute,
    TooLong,
    BadGuestEnv,
    Bindings(BindingsError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRootfs => write!(f, "{} is not set", env::ROOTFS),
            Self::RootfsNotAbsolute => write!(f, "{} must be an absolute path", env::ROOTFS),
            Self::TooLong => write!(f, "an engine variable exceeds {MAX_PATH} bytes"),
            Self::BadGuestEnv => write!(f, "{} entries must look like KEY=VALUE", env::GUEST_ENV),
            Self::Bindings(error) => write!(f, "{}: {error:?}", env::BINDS),
        }
    }
}

pub fn get() -> &'static Config {
    // SAFETY: see `Global`.
    unsafe { &*CONFIG.get() }
}

pub fn translator() -> Option<&'static Translator<'static>> {
    get().translator.as_ref()
}

/// Copy the engine variables out of `entries` and build the translator.
pub fn init(
    entries: impl Iterator<Item = &'static [u8]>,
    page_size: usize,
) -> Result<(), InitError> {
    let config = CONFIG.get();
    // SAFETY: single-threaded startup; nothing else holds a reference.
    let config = unsafe { &mut *config };
    config.page_size = page_size;
    for entry in entries {
        if let Some(value) = env_value(entry, env::ROOTFS.as_bytes()) {
            config.rootfs.set(value)?;
        } else if let Some(value) = env_value(entry, env::BINDS.as_bytes()) {
            config.binds.set(value)?;
        } else if let Some(value) = env_value(entry, env::LOG.as_bytes()) {
            config.log.set(value)?;
        } else if let Some(value) = env_value(entry, env::FAKE_ROOT.as_bytes()) {
            config.fake_root = value == b"1";
        } else if let Some(value) = env_value(entry, env::PROGRAM.as_bytes()) {
            config.program.set(value)?;
        } else if let Some(value) = env_value(entry, env::SELFTEST.as_bytes()) {
            config.selftest = value == b"1";
        } else if let Some(value) = env_value(entry, env::GUEST_ENV.as_bytes()) {
            for item in value.split(|&byte| byte == LIST_SEPARATOR as u8) {
                if !item.is_empty() {
                    config.guest_env.add(item)?;
                }
            }
        }
    }
    if config.rootfs.is_empty() {
        return Err(InitError::MissingRootfs);
    }
    if !is_absolute(config.rootfs.as_bytes()) {
        return Err(InitError::RootfsNotAbsolute);
    }

    let mut exe = [0u8; MAX_PATH];
    if let Ok(len) = sys::readlinkat(sys::AT_FDCWD, c"/proc/self/exe".as_ptr().cast(), &mut exe) {
        config.loader_path.set(&exe[..len])?;
    }

    // The translator borrows the buffers for the rest of the process;
    // they live in this static and are never written again.
    let rootfs: &'static [u8] =
        unsafe { core::slice::from_raw_parts(config.rootfs.as_ptr(), config.rootfs.len) };
    let binds: &'static [u8] =
        unsafe { core::slice::from_raw_parts(config.binds.as_ptr(), config.binds.len) };
    let mut bindings = Bindings::with_defaults();
    bindings.parse_spec(binds).map_err(InitError::Bindings)?;
    config.translator = Some(Translator::new(rootfs, bindings));
    Ok(())
}

/// Record where the guest's auxiliary vector lives (inside the vector
/// built below the kernel's, which stays mapped for the process life).
pub fn set_auxv(ptr: usize, len: usize) {
    let config = CONFIG.get();
    // SAFETY: still single-threaded; the guest has not started.
    let config = unsafe { &mut *config };
    config.auxv_ptr = ptr;
    config.auxv_len = len;
}

/// Record the guest-visible path of the executable the loader mapped.
pub fn set_guest_exe(path: &[u8]) {
    let config = CONFIG.get();
    // SAFETY: still single-threaded; the guest has not started.
    let config = unsafe { &mut *config };
    if config.guest_exe.set(path).is_err() {
        // A path this long could not have been opened either.
    }
}
