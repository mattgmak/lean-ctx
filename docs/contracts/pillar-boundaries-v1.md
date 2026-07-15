# Pillar Boundaries Contract v1

> Architectural contract defining the three lean-ctx pillars and their
> dependency rules. CI-enforced via `contracts_frozen.rs`.

## Pillars

### Engine (always compiled)

The developer-facing context compression layer. All features work locally,
offline, with zero telemetry.

**Top-level modules:** `core`, `tools`, `server`, `engine`, `tool_defs`,
`instructions`, `mcp_stdio`, `hooks`, `hook_handlers`, `rules_inject`,
`rewrite_registry`, `shell`, `shell_hook`, `dashboard`, `tui`, `terminal_ui`,
`lsp`, `compound_lexer`, `marked_block`, `dropin`, `heatmap`, `token_report`,
`daemon`, `daemon_autostart`, `daemon_client`.

**Feature flags:** none (always compiled).

### Gateway (feature: `http-server` + `gateway-server`)

The org-wide LLM reverse proxy with usage tracking, budget enforcement, and
FinOps dashboards.

**Top-level modules:** `proxy`, `proxy_autostart`, `proxy_setup`,
`gateway_server`.

**Feature flags:** `http-server` (proxy binary), `gateway-server` (admin +
usage store).

### Cloud (feature: `cloud-server`)

Hosted coordination: accounts, team provisioning, knowledge sync, billing
edge, context package registry.

**Top-level modules:** `cloud_server`, `cloud_client`, `cloud_sync`,
`http_server`.

**Feature flags:** `cloud-server`, `http-server` (shared transport),
`team-server` (team/billing submodules in `http_server`).

### Shared (always compiled)

CLI, IPC, config, diagnostics — consumed by all three pillars.

**Top-level modules:** `cli`, `config_io`, `ipc`, `doctor`, `setup`,
`status`, `report`, `uninstall`.

## Dependency rules

1. **Engine depends on nothing** — it is the foundation.
2. **Gateway depends on Engine** — the proxy compresses prompts using
   `core::compressor`.
3. **Cloud depends on Engine** — sync and billing reference `core::config`,
   `core::savings_ledger`.
4. **Gateway ↔ Cloud are independent** — no direct imports between
   `proxy`/`gateway_server` and `cloud_server`/`cloud_sync`.
5. **`http_server` is cross-pillar** — it hosts both Engine HTTP MCP
   transport and Cloud team surfaces. Submodules are documented with their
   pillar assignment in `http_server/mod.rs`.

## Cross-pillar coupling (documented exceptions)

### proxy ↔ gateway_server

The self-hosted org gateway runs as a single process:
- `gateway_server::serve` calls `proxy::start_proxy`
- `proxy` mounts `gateway_server::user_api` and `gateway_server::mcp::proxy`
  routes (feature-gated)

This bidirectional dependency is intentional and documented in both `mod.rs`
files.

## Local-Free Invariant

Every feature in every pillar works self-hosted for free. Commercial tiers
(Cloud) add hosting and support, never capabilities. CI enforces this via
the `local_free_invariant` test.

## Naming convention

| Old name | New name | Reason |
|----------|----------|--------|
| `core::gateway` | `core::mcp_catalog` | Avoid collision with `gateway_server` (the LLM Gateway) |
