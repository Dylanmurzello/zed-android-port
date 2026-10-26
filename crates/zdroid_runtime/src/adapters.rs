//! Concrete provider implementations. Each adapter satisfies
//! [`crate::port::RuntimeProvider`] and is selected at runtime via
//! [`crate::config::ResolvedConfig`].
//!
//! - [`rootfs`]: a stock Ubuntu rootfs run through the engine loader.
//! - [`chroot`]: talks to `zd-spawnd` over a Unix socket; needs root and
//!   the Magisk module, kept until the rootfs adapter carries the daily
//!   workload.

pub mod chroot;
pub mod rootfs;
pub mod rootfs_install;

use std::path::Path;

use crate::config::ResolvedConfig;
use crate::port::RuntimeProvider;

/// Pick the adapter for the resolved config. `data_path` is the app
/// files dir; adapters derive every host-side location from it.
pub fn for_config(config: &ResolvedConfig, data_path: &Path) -> anyhow::Result<Box<dyn RuntimeProvider>> {
    match config {
        ResolvedConfig::Chroot(cfg) => chroot::ChrootAdapter::new(cfg.clone()).map(box_it),
        ResolvedConfig::Rootfs(cfg) => Ok(box_it(rootfs::RootfsAdapter::new(cfg.clone(), data_path))),
    }
}

fn box_it<T: RuntimeProvider + 'static>(adapter: T) -> Box<dyn RuntimeProvider> {
    Box::new(adapter)
}
