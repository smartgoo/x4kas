//! Everything below the frontends: node RPC, app state, analytics and config. Shared by
//! the `x4kas` GUI and the `x4kas-cli` binary, and free of GUI and CLI-parsing code.

pub mod analytics;
pub mod analytics_streaming;
pub mod app;
pub mod config;
pub mod controller;
pub mod format;
pub mod polling;
pub mod rpc;
pub mod tx_inspect;
