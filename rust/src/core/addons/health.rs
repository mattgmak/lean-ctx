//! Post-install health probe (#1076).
//!
//! After an addon is wired, [`probe`] connects to its MCP server exactly the way
//! the gateway will (resolve → spawn under the same sandbox → MCP `initialize` →
//! `tools/list`) and reports the discovered tools. This turns a broken
//! `command`/`args` (or under-provisioned capabilities) into a clear failure at
//! install time instead of an opaque error at first `ctx_tools` use.
//!
//! Impure by nature (spawns a process / opens a connection), so it lives outside
//! the pure [`super::install`] path and is driven from the CLI.

use std::time::Duration;

use crate::core::mcp_catalog::{GatewayServer, client};

/// What a successful [`probe`] found on the downstream server.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// Number of tools the server advertised via `tools/list`.
    pub tool_count: usize,
    /// Tool names, sorted (for a stable, human-friendly summary).
    pub tools: Vec<String>,
}

/// Connect to `server` and list its tools, bounded by `timeout`. Returns a
/// [`ProbeReport`] on success or a human-readable reason it could not be reached
/// (spawn failure, handshake failure, timeout, sandbox block, …).
pub fn probe(server: &GatewayServer, timeout: Duration) -> Result<ProbeReport, String> {
    let resolved = server.resolve()?;
    // The CLI has no ambient Tokio runtime; build a one-shot current-thread one
    // (the same approach `ctx_tools` uses for its CLI call path).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start runtime for the health probe: {e}"))?;
    let tools = rt.block_on(client::fetch_tools(&resolved, timeout))?;
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    Ok(ProbeReport {
        tool_count: names.len(),
        tools: names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp_catalog::TransportKind;

    #[test]
    fn probe_reports_a_clear_error_for_a_missing_binary() {
        // A command that does not exist must surface a readable spawn failure
        // rather than panicking — the whole point of the install-time probe.
        let server = GatewayServer {
            name: "ghost".into(),
            transport: TransportKind::Stdio,
            command: "lean-ctx-no-such-mcp-binary-xyz".into(),
            ..Default::default()
        };
        let err = probe(&server, Duration::from_secs(5)).expect_err("missing binary must fail");
        assert!(!err.is_empty());
    }

    #[test]
    fn probe_rejects_an_unresolvable_server() {
        // stdio transport without a command can't resolve → clear error.
        let server = GatewayServer {
            name: "broken".into(),
            transport: TransportKind::Stdio,
            ..Default::default()
        };
        assert!(probe(&server, Duration::from_secs(1)).is_err());
    }
}
