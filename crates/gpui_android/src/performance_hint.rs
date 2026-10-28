//! A performance hint session (ADPF) for the run loop's frames.
//!
//! Android reports frame deadlines to the CPU scheduler for apps that draw
//! through its UI toolkit. Zdroid paints with Vulkan on the `android_main`
//! thread, so the scheduler only sees its load and may leave a paint on a
//! slow core. Reporting how long each frame took against the frame budget
//! lets it raise the clock or move the thread to a faster core when frames
//! run late. See `docs/workarounds/choreographer-vsync.md`.

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Duration;

/// The `APerformanceHint_*` functions are NDK API 33+, and minSdk is 26,
/// so they're resolved at runtime like `ANativeWindow_setFrameRate`.
type GetManagerFn = unsafe extern "C" fn() -> *mut c_void;
type CreateSessionFn = unsafe extern "C" fn(
    manager: *mut c_void,
    thread_ids: *const i32,
    thread_count: usize,
    initial_target_work_duration_nanos: i64,
) -> *mut c_void;
type ReportActualWorkDurationFn =
    unsafe extern "C" fn(session: *mut c_void, actual_duration_nanos: i64) -> i32;
type CloseSessionFn = unsafe extern "C" fn(session: *mut c_void);

struct Session {
    handle: *mut c_void,
    report_actual_work_duration: ReportActualWorkDurationFn,
    close: CloseSessionFn,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { (self.close)(self.handle) };
    }
}

thread_local! {
    /// `None` until the first frame, then `Some(None)` where hint
    /// sessions aren't available. Per thread, because `android_main`
    /// runs on a new thread when the Activity is recreated; the old
    /// thread's session closes when that thread exits.
    static SESSION: RefCell<Option<Option<Session>>> = const { RefCell::new(None) };
}

/// Reports how long the run loop spent on a frame. Call on the
/// `android_main` thread.
pub(crate) fn frame_finished(work_duration: Duration) {
    SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let slot = session.get_or_insert_with(create_session);
        let Some(active) = slot else {
            return;
        };
        // The call rejects zero; a frame that drew nothing still took some time.
        let nanos = i64::try_from(work_duration.as_nanos())
            .unwrap_or(i64::MAX)
            .max(1);
        let result = unsafe { (active.report_actual_work_duration)(active.handle, nanos) };
        if result != 0 {
            log::warn!(
                "APerformanceHint_reportActualWorkDuration returned {result}; \
                 closing the hint session"
            );
            *slot = None;
        }
    });
}

fn create_session() -> Option<Session> {
    let lib = unsafe {
        libc::dlopen(
            c"libandroid.so".as_ptr(),
            libc::RTLD_NOLOAD | libc::RTLD_LAZY,
        )
    };
    if lib.is_null() {
        log::warn!("performance hints: libandroid.so not loaded");
        return None;
    }
    let symbol = |name: &std::ffi::CStr| {
        let address = unsafe { libc::dlsym(lib, name.as_ptr()) };
        (!address.is_null()).then_some(address)
    };
    let (Some(get_manager), Some(create), Some(report), Some(close)) = (
        symbol(c"APerformanceHint_getManager"),
        symbol(c"APerformanceHint_createSession"),
        symbol(c"APerformanceHint_reportActualWorkDuration"),
        symbol(c"APerformanceHint_closeSession"),
    ) else {
        log::info!("performance hints unavailable (API < 33)");
        return None;
    };
    let get_manager: GetManagerFn = unsafe { std::mem::transmute(get_manager) };
    let create: CreateSessionFn = unsafe { std::mem::transmute(create) };
    let report: ReportActualWorkDurationFn = unsafe { std::mem::transmute(report) };
    let close: CloseSessionFn = unsafe { std::mem::transmute(close) };

    let manager = unsafe { get_manager() };
    if manager.is_null() {
        log::info!("performance hints unavailable: no hint manager");
        return None;
    }
    let thread_id = unsafe { libc::gettid() };
    let target_nanos = (1e9 / crate::platform::TARGET_FRAME_RATE_HZ as f64) as i64;
    let handle = unsafe { create(manager, &thread_id, 1, target_nanos) };
    if handle.is_null() {
        // Devices without hint support in their power HAL return null.
        log::info!("performance hints unavailable: the device doesn't support hint sessions");
        return None;
    }
    log::info!("performance hint session for thread {thread_id}, target {target_nanos}ns");
    Some(Session {
        handle,
        report_actual_work_duration: report,
        close,
    })
}
