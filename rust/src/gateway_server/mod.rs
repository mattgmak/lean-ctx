//! Self-hosted org gateway — the **Gateway pillar** deployment mode.
//!
//! Bundles the LLM proxy (`crate::proxy`), the per-request usage store
//! (Postgres `usage_events`), and the admin console into one deployable
//! server process.
//!
//! # Cross-pillar coupling
//!
//! `serve.rs` calls `proxy::start_proxy` to start the LLM proxy and wires
//! `proxy::usage_sink` for async Postgres persistence. The proxy in turn
//! mounts `gateway_server::user_api` and `gateway_server::mcp::proxy` routes
//! (feature-gated). This bidirectional dependency is intentional: the
//! gateway is a single process, not two services.
//!
//! # Invariants
//!
//! - **Local-Free**: compiled in or out via `--features gateway-server`,
//!   never gated by account/license/plan (Local-Free Invariant).
//! - **Fail-open**: a slow or down Postgres degrades metering, never live
//!   LLM traffic.

pub mod admin_api;
pub mod admin_status;
pub mod admin_timeseries;
pub mod admin_ui;
pub mod doctor;
pub mod evidence;
pub mod init;
pub mod keys_cli;
pub mod mcp;
pub mod report;
pub mod security;
pub mod serve;
pub mod store;
pub mod user_api;
