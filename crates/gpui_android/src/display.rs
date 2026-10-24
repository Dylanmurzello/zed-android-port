use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay, point, px, size};
use std::fmt;
use std::time::Duration;
use uuid::Uuid;

pub(crate) struct AndroidDisplay;

impl AndroidDisplay {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn uuid(&self) -> anyhow::Result<Uuid> {
        Ok(Uuid::nil())
    }

    fn bounds(&self) -> Bounds<Pixels> {
        // Real bounds populated from ANativeWindow size in Phase 7.4. For now return a
        // landscape-tablet-sized default so windowing math doesn't divide by zero.
        Bounds::new(point(px(0.0), px(0.0)), size(px(1920.0), px(1080.0)))
    }

    fn refresh_interval(&self) -> Option<Duration> {
        None
    }
}

impl fmt::Debug for AndroidDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AndroidDisplay").finish()
    }
}
