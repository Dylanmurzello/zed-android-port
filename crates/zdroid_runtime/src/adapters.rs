//! Concrete provider implementations. Each adapter satisfies
//! [`crate::port::RuntimeProvider`] and is selected at runtime via
//! [`crate::config::ResolvedConfig`].
//!
//! Module layout:
//!
//! - [`chroot`] — talks to `zd-spawnd` over a Unix socket, fastest path,
//!   needs the Magisk module installed.
//! - [`bootstrap`] — owns its own `$PREFIX` (downloaded from a separate
//!   GitHub repo), dispatches via direct exec or proot.
//!
//! Each module starts as a stub; concrete implementations land in
//! their own commits.

pub mod bootstrap;
pub mod bootstrap_install;
pub mod chroot;

use crate::config::ResolvedConfig;
use crate::port::RuntimeProvider;

/// Factory: pick the right adapter for the resolved config. Returns
/// the boxed trait object the rest of the system uses.
///
/// Adapters that haven't been implemented yet return a deliberate
/// "not yet implemented" error — settings UI surfaces this as a
/// greyed-out option with a tooltip, rather than crashing.
pub fn for_config(config: &ResolvedConfig) -> anyhow::Result<Box<dyn RuntimeProvider>> {
    match config {
        ResolvedConfig::Chroot(cfg) => chroot::ChrootAdapter::new(cfg.clone()).map(box_it),
        ResolvedConfig::Bootstrap(cfg) => {
            bootstrap::BootstrapAdapter::new(cfg.clone()).map(box_it)
        }
    }
}

fn box_it<T: RuntimeProvider + 'static>(adapter: T) -> Box<dyn RuntimeProvider> {
    Box::new(adapter)
}
