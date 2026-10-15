# LeanCTX and Repomix

> **Status: Historical / Research.** This retained record is not current
> onboarding or a release claim. LeanCTX is the **Context Gateway for AI Systems**.
> See [current positioning](../POSITIONING_CANONICAL.md) and the relevant edition's
> released contract; older product definitions and availability below are historical.

Repomix creates a shareable snapshot of a repository. LeanCTX is a local context
Runtime for existing coding agents: it can expose selected project material via
CLI, MCP, and a local proxy before model inference. They address different
integration shapes, not a benchmark race.

| Need | Evaluate |
|---|---|
| A single repository export for a one-off prompt | Repomix's packing workflow |
| Agent-facing local selection and read shaping during a coding session | LeanCTX Runtime with its documented read modes |
| A repeatable decision | A comparable trial on the same repository, task, model, and quality threshold |

## Evidence boundary

Historical drafts on this page contained tool counts, cached-token figures, and
percentage reductions. Those figures are not retained as public claims: the
result depends on the chosen mode, source material, agent behavior, and task.
Only a declared baseline and treatment with a visible quality threshold can
support a gain claim.

## Current boundary

LeanCTX is **The Context SDK for AI Agents**. It is not a repository-packing
replacement claim, agent platform, hosted service, or generic agent builder.
Available local primitives and their exact status are listed in the internal
Product Architecture (`docs/internal/vision/PRODUCT-ARCHITECTURE.md`, internal — not in this repository).
