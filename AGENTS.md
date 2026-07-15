# Context Engineering Layer

lean-ctx optimizes LLM context by compressing file reads, shell output, and search results.

## Integration Mode: Replace

Native Read/Grep/Glob/Shell are **denied by policy**. lean-ctx MCP tools are the
only path for reading files and running commands:

- **Reads/Search** → `ctx_read`, `ctx_search`, `ctx_compose` (cached, 10 modes)
- **Shell commands** → `ctx_shell` (95+ compression patterns)
- **File editing** → native Edit/StrReplace (lean-ctx only handles READ operations)
- **File finding** → `ctx_glob`, `ctx_tree`

Native tools will return a deny error with instructions to use ctx_* equivalents.
Override: set `hook_mode = "hybrid"` in `~/.config/lean-ctx/config.toml`.

## Subagent Compatibility (Cursor / Claude Code)

Cursor blocks MCP tools in `readonly: true` subagents (Task tool).
lean-ctx tools are MCP tools → they WILL FAIL in readonly subagents.

**Rules for spawning subagents:**
- Always set `readonly: false` when the subagent needs ctx_* tools
- Use `subagent_type: "generalPurpose"` — NOT `"explore"` (always readonly)
- lean-ctx tools declare `readOnlyHint: true` in MCP annotations

**Fallback (MCP unavailable):** deny hook detects MCP-down and allows native tools.

## CLI commands (optimized shell, lower overhead)

```bash
git status                   # compressed by configured agent shell/wrapper
cargo test                   # compressed by configured agent shell/wrapper
lean-ctx -c "git status"     # only if explicitly requested / documented unwrapped
lean-ctx ls src/              # directory map
```

## Development Workflow

When working on lean-ctx itself:

1. **Before building**: `lean-ctx stop` (LaunchAgent respawns otherwise)
2. **Build**: `cd rust && cargo build --release`
3. **Test**: `cargo test --lib` + `cargo clippy -- -W clippy::all`
4. **Install**: `lean-ctx dev-install` (atomic stop→build→install→restart)

## Session Continuity

lean-ctx automatically persists session context across restarts:
- **Findings**: Recent tool results (reads, searches, test outcomes)
- **Decisions**: Architecture choices made during the session
- **Files**: Touched files with summaries and modification status
- **Progress**: Task completion state and next steps

This data is delivered through the first tool call's `--- AUTO CONTEXT ---`
briefing (default `minimal_overhead = true`: initialize instructions stay
byte-stable for provider prompt caching, #498). With `minimal_overhead = false`
it is additionally injected at session start via the `ACTIVE SESSION` LITM block.

### Active Documentation (Agent Responsibility)

After completing a significant task (implementation, bugfix, refactoring):
1. Record the decision: `ctx_knowledge(action="remember", category="decision", content="...")`
2. Record progress: `ctx_session(action="task", value="<current task> [N%]")`
3. Record blockers: `ctx_knowledge(action="remember", category="blocker", content="...")`

After 30+ tool calls without documentation:
- lean-ctx will prompt with `[CHECKPOINT: please document current progress]`
- Respond by calling `ctx_session(action="task")` with current status

## Provider Pipeline (Context Engine)

External data sources (GitHub, GitLab, Jira, Postgres, MCP bridges, custom REST) are first-class citizens.
All provider data flows through the same consolidation pipeline:

1. `ContextProvider::execute()` → raw `ProviderResult`
2. `consolidation::consolidate()` → `ConsolidationArtifacts` (BM25 chunks, graph edges, knowledge facts, cache entries)
3. `apply_artifacts_to_stores()` → persists to BM25 index, Graph index, ProjectKnowledge, Session cache (background thread)

This means `ctx_semantic_search` finds issues/PRs/tickets, `ctx_knowledge` recalls provider facts,
and `ctx_read` shows cross-source hints (e.g. "Issue #42 references this file").

## Quality Bar

- Zero clippy warnings, all tests pass
- Security: PathJail, Shell Allowlist, bounded_lock, no hardcoded secrets
- No mock data, no placeholders, no stubs

## Output Determinism (#498)

Tool outputs MUST be deterministic functions of (file content, mode, CRP mode, task).
Provider-side prompt caching (Anthropic 90%, OpenAI 50% discount) rewards byte-stable text;
any timestamp, counter or random element in tool output bodies defeats it.

- No timestamps/counters in output bodies. Artifact paths are content-addressed
  (see `save_tee`: `{cmd_slug}_{blake3(cmd)[..8]}.log`).
- Dynamic additions (hints, checkpoints) only as state-triggered suffixes with stable headers.
- Regression guard: determinism tests in `ctx_read/tests.rs`, `ctx_search.rs`, `shell/redact.rs`.

<!-- lean-ctx -->
## lean-ctx

lean-ctx is active — the MCP tools replace native equivalents.
Full rules: LEAN-CTX.md (open on demand — do not auto-load).
<!-- /lean-ctx -->
<!-- lean-ctx-compression -->
OUTPUT STYLE: concise
- Bullet points over paragraphs
- Skip filler words and hedging ("I think", "probably", "it seems")
- 1-sentence explanations max, then code/action
- No repeating what the user said
<!-- /lean-ctx-compression -->
