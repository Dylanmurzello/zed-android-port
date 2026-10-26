//! Per-frame timing instrumentation. Three samples per frame:
//!
//! - **vsync_interval**: time between successive Choreographer callbacks
//!   while frames run back to back. At 120Hz the target is 8.33ms.
//!   Variance is jitter from the looper (spurious wakes, scheduler
//!   preemption, thermal throttling). Frames are only requested while
//!   something changes, so a frame requested more than [`IDLE_GAP`] after
//!   the previous vsync starts a new run instead of counting as slow.
//! - **vsync_to_paint**: latency from the Choreographer callback firing
//!   to the main loop reaching `window.refresh()`. This is the cost of
//!   the poll-events + input-drain pipeline between waking up on vsync
//!   and starting to paint.
//! - **paint_duration**: time spent inside `window.refresh()` (all
//!   visible windows). The work itself.
//!
//! Every 240 frames (~2s at 120Hz) we sort the samples and log P50/P95/P99
//! + max at debug level, so they stay out of `Zed.log` unless asked for:
//! `"log": {"gpui_android::frame_timing": "debug"}` in settings. Sort is
//! in-place on a `Vec` with reserved capacity so steady-state allocation is
//! zero.
//!
//! All entry points are main-thread only (Choreographer callback +
//! `AndroidPlatform::run` loop both run on the activity thread), so
//! `thread_local!` storage is sufficient.

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// Sample window before flushing percentile stats. 240 ≈ 2 seconds at
/// 120Hz, ≈ 4 seconds at 60Hz. Chosen to give a stable percentile
/// estimate without dragging the trailing edge of stats too far behind
/// what the user just saw.
const SAMPLE_WINDOW: usize = 240;

/// A frame requested this long after the previous vsync follows an idle
/// pause. Slow frames still show up in `paint_duration`.
const IDLE_GAP: Duration = Duration::from_millis(50);

thread_local! {
    static STATE: RefCell<FrameTimingState> = RefCell::new(FrameTimingState::new());
}

struct FrameTimingState {
    last_vsync: Option<Instant>,
    next_vsync_continues_run: bool,
    vsync_awaiting_paint: Option<Instant>,
    pending_paint_start: Option<Instant>,
    vsync_intervals_ns: Vec<u64>,
    vsync_to_paint_ns: Vec<u64>,
    paint_durations_ns: Vec<u64>,
}

impl FrameTimingState {
    fn new() -> Self {
        Self {
            last_vsync: None,
            next_vsync_continues_run: false,
            vsync_awaiting_paint: None,
            pending_paint_start: None,
            vsync_intervals_ns: Vec::with_capacity(SAMPLE_WINDOW),
            vsync_to_paint_ns: Vec::with_capacity(SAMPLE_WINDOW),
            paint_durations_ns: Vec::with_capacity(SAMPLE_WINDOW),
        }
    }

    fn record_frame_request(&mut self) {
        self.next_vsync_continues_run = self
            .last_vsync
            .is_some_and(|prev| prev.elapsed() < IDLE_GAP);
    }

    fn record_vsync(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_vsync
            && std::mem::take(&mut self.next_vsync_continues_run)
        {
            self.vsync_intervals_ns
                .push(now.duration_since(prev).as_nanos() as u64);
        }
        self.last_vsync = Some(now);
        self.vsync_awaiting_paint = Some(now);
    }

    fn record_paint_start(&mut self) {
        let now = Instant::now();
        if let Some(vsync) = self.vsync_awaiting_paint.take() {
            self.vsync_to_paint_ns
                .push(now.duration_since(vsync).as_nanos() as u64);
        }
        self.pending_paint_start = Some(now);
    }

    fn record_paint_end(&mut self) {
        if let Some(start) = self.pending_paint_start.take() {
            self.paint_durations_ns
                .push(Instant::now().duration_since(start).as_nanos() as u64);
        }
        if self.paint_durations_ns.len() >= SAMPLE_WINDOW {
            self.flush_stats();
        }
    }

    fn flush_stats(&mut self) {
        log_percentiles("vsync_interval", &mut self.vsync_intervals_ns);
        log_percentiles("vsync_to_paint", &mut self.vsync_to_paint_ns);
        log_percentiles("paint_duration", &mut self.paint_durations_ns);
        self.vsync_intervals_ns.clear();
        self.vsync_to_paint_ns.clear();
        self.paint_durations_ns.clear();
    }
}

fn log_percentiles(name: &str, samples: &mut Vec<u64>) {
    if samples.is_empty() {
        return;
    }
    samples.sort_unstable();
    let n = samples.len();
    let p50 = samples[n / 2];
    let p95 = samples[(n * 95 / 100).min(n - 1)];
    let p99 = samples[(n * 99 / 100).min(n - 1)];
    let max = *samples.last().unwrap();
    log::debug!(
        "frame_timing[{name}]: n={n} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms",
        p50 as f64 / 1e6,
        p95 as f64 / 1e6,
        p99 as f64 / 1e6,
        max as f64 / 1e6,
    );
}

/// Call when a Choreographer callback is posted, to tell a frame that
/// continues a run of frames from one that ends an idle pause.
pub(crate) fn frame_requested() {
    STATE.with(|s| s.borrow_mut().record_frame_request());
}

/// Call from the Choreographer frame callback. Records the vsync
/// arrival timestamp and, within a run of frames, the interval since the
/// previous vsync.
pub(crate) fn vsync_arrived() {
    STATE.with(|s| s.borrow_mut().record_vsync());
}

/// Call immediately before `window.refresh()` starts. Captures the
/// vsync-to-paint-start latency.
pub(crate) fn paint_started() {
    STATE.with(|s| s.borrow_mut().record_paint_start());
}

/// Call immediately after the refresh block finishes. Captures paint
/// duration and triggers a percentile flush if the sample window is
/// full.
pub(crate) fn paint_finished() {
    STATE.with(|s| s.borrow_mut().record_paint_end());
}
