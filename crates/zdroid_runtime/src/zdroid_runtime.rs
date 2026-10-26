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
//! - [`config`] — `zd-runtime.toml` schema + serde, plus the
//!   [`config::ResolvedConfig`] that downstream code actually consumes.
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
pub mod health;
pub mod port;

pub use config::{ResolvedConfig, RuntimeId};
pub use health::{HealthStatus, ProgressSink};
pub use port::{RuntimeProvider, SpawnHandle, SpawnRequest};
