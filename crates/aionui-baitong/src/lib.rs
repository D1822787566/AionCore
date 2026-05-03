#![warn(clippy::disallowed_types)]

//! Baitong local-adapter bridge (phase 1).
//!
//! AionCore owns the adapter process and is the single runtime source for its
//! configuration. AionUi talks only to this crate's HTTP surface.

mod client;
mod error;
mod launch;
pub mod managed;
pub mod routes;
mod service;
mod supervisor;

pub use client::{AdapterClient, AdapterState};
pub use error::BaitongError;
pub use launch::AdapterLaunch;
pub use managed::{ManagedSpec, VisionMcp, spec_from_status};
pub use routes::{BaitongRouterState, baitong_routes};
pub use service::{BaitongService, ManagedProviderSink};
pub use supervisor::{AdapterSupervisor, DEFAULT_CONTROL_PORT};
