# LeanCTX Runtime

> **Status: implementation guide.** LeanCTX is the **Context Gateway for AI Systems**.
> **Control what your AI can see.** LeanCTX Engine is the runtime in this
> directory; LeanCTX SDK is the separate application integration surface.
> Local CLI, MCP, hooks, supported proxy paths, and bounded context evidence
> are available. Standalone SDK stable and Preview namespaces have their own
> released contracts. Commercial Enterprise capabilities have separate scope
> and licensing. See [current positioning](../docs/POSITIONING_CANONICAL.md).

## Build locally

```bash
cd rust
cargo build --release
```

This build writes only to the worktree. Do not stop an installed Runtime before
building or testing. For a local development install, use `lean-ctx dev-install`
after the checks below succeed.

## Validate a change

```bash
cd rust
cargo test --lib
cargo clippy --all-features -- -D warnings
cargo fmt --check
```

## Use the local Runtime

```bash
lean-ctx setup
lean-ctx doctor
lean-ctx wrap codex
```

The Runtime supports context selection, compression, reuse, recovery, and
local evidence. Measure a result against a comparable baseline and treatment,
declare a quality threshold, and keep the methodology visible. A cheaper failed
task is not a gain.

## Integration boundaries

- **Attach:** use the installed CLI, MCP server, or local proxy.
- **Wrap:** use the declared adapter path where available; discover capabilities
  and report typed limitations when a capability is unavailable.
- **Embed:** use the standalone LeanCTX SDK's released lifecycle and Agent Tools
  contracts. Its Python Preview namespace and older in-tree experiments retain
  their separate limits.

See the repository [README](../README.md) for installation and current public
orientation. See [canonical positioning](../docs/POSITIONING_CANONICAL.md)
and [SDK boundaries](../docs/reference/sdk-surface.md) for current scope.
