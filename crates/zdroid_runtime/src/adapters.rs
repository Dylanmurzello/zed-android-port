pub mod bootstrap;
pub mod bootstrap_install;
pub mod chroot;

use crate::config::ResolvedConfig;
use crate::port::RuntimeProvider;

pub fn for_config(config: &ResolvedConfig) -> anyhow::Result<Box<dyn RuntimeProvider>> {
    match config {
        ResolvedConfig::Chroot(cfg) => chroot::ChrootAdapter::new(cfg.clone()).map(box_it),
        ResolvedConfig::Bootstrap(cfg) => bootstrap::BootstrapAdapter::new(cfg.clone()).map(box_it),
        ResolvedConfig::ExternalTermux(_) => {
            anyhow::bail!(
                "External Termux is not supported. Select Bootstrap or Chroot in Android Runtime settings."
            )
        }
    }
}

fn box_it<T: RuntimeProvider + 'static>(adapter: T) -> Box<dyn RuntimeProvider> {
    Box::new(adapter)
}
