//! Zdroid runtime port + adapters.
//!
//! The editor never calls a userland directly. It calls into this
//! crate's [`RuntimeProvider`] port, and the active adapter handles
//! the actual spawn. The selection lives in `zd-runtime.toml`.
//!
//! # Crate layout
//!
//! - [`port`] — the [`RuntimeProvider`] trait + spawn types. The contract
//!   every adapter satisfies.
//! - [`config`] — `zd-runtime.toml` schema, location, and the
//!   [`config::RuntimeState`] the editor boots from.
//! - [`env`] — the baseline Zed-process env every adapter extends.
//! - [`health`] — [`health::HealthStatus`] feasibility-ping primitive +
//!   the [`health::ProgressSink`] hook used by long-running install ops.
//! - [`adapters`] — concrete provider impls, one submodule each.
//!
//! # Dispatch
//!
//! ```text
//! Zed editor ───► PATH lookup ───► <data>/files/zd-runtime/<name> ───► zd-exec
//!                                                                        │
//!                                                                        ▼
//!                                                 reads zd-runtime.toml, picks the
//!                                                 adapter, calls RuntimeProvider::spawn
//! ```

pub mod adapters;
pub mod config;
pub mod env;
pub mod health;
pub mod port;

pub use config::{ResolvedConfig, RuntimeId};
pub use health::{HealthStatus, ProgressSink};
pub use port::{RuntimeProvider, SpawnHandle, SpawnRequest};
