# ADR-015: Evidence-Aware Semantic Code Intelligence

**Status:** Accepted
**Date:** 2026-10-02
**Authors:** Architecture Team

## Context

lean-ctx builds its code graph from tree-sitter. Tree-sitter is fast, local,
dependency-free and works on broken code, but it only sees syntax: it extracts
a call to `save`, not *which* `save`. Calls were bound to definitions by name
within the caller's scope (same file → unique import → unique project-wide);
anything else was dropped. Two producers had additionally been guessing:
graph enrichment mapped names to files last-wins, and the `ctx_impact` index
picked the alphabetically first definition. Every guess became a `calls` edge
that ranking, impact analysis and context selection treated as fact.

Separately, `ctx_refactor` already talked to local language servers and a live
JetBrains IDE through `lsp::router`, but that semantic knowledge never reached
the graph.

## Decision

### 1. Tree-sitter stays the always-available baseline

Nothing requires a language server. Without one, the graph is the structural
graph — minus the former guesses.

### 2. Edges carry typed evidence

`Edge.metadata` holds `EdgeEvidence { v, grade, by: [Contribution] }`, one
contribution `{ origin, grade, backend, sites }` per producer currently
deriving the edge; `grade` is the strongest:

| Grade | Meaning |
|---|---|
| `verified_semantic` | a local language server / IDE resolved the target |
| `resolved_structural` | bound by the caller's own scope (same file, unique import) |
| `heuristic_structural` | name unique in the project, but not in the caller's scope |

Ranking and impact weight an edge by `kind weight × grade factor` (heuristic
0.5, otherwise 1.0); edges without evidence keep their full weight. No
timestamps, no source text: metadata stays deterministic.

Each producer (graph enrichment, `ctx_impact` index) replaces or withdraws
only its own contribution; an edge is deleted only when no producer derives
it any more, so one producer can never downgrade or erase another's evidence.

A producer withdraws an edge only when its pass *settled* the question: an
unanswered site (backend unavailable or busy, budget exhausted, timeout, cold
server) is not evidence against an earlier verified edge. A file that no
longer declares any trait/interface is settled, so edges into it are
withdrawn. Read-merge-write of evidence runs under `BEGIN IMMEDIATE`, so two
processes updating the same edge cannot lose a contribution.

### 3. Escalate only uncertainty

Only call sites structure cannot bind — an ambiguous name, a name merely unique
project-wide, or a path call (`db::save`) — are sent to the semantic backend,
at the callee identifier's exact position (tree-sitter byte column encoded per
the negotiated LSP position encoding, UTF-8 preferred). Scope-bound calls and
names with no project definition are never queried.

A verified target binds the edge; a definition outside the indexed project
vetoes a structural guess. "No definition found" is **not** evidence — a cold
server answers it while indexing — so it is neither cached nor used as a veto.

The question asked is `textDocument/definition` at the callee, not the call
hierarchy: one request per site answers exactly "where does this call go",
every backend offers it (pylsp has no call hierarchy, the JetBrains bridge no
endpoint for it), and the evals verify every ambiguous call with it.
`callHierarchy/outgoingCalls` would answer per *caller* — all its calls,
certain ones included — at several times the cost for the same edges.

### 4. Cache definitive answers, validate on reuse

Answers are cached per call site in the property graph. A row is reused only
while (a) the caller file's content hash is unchanged, (b) its context
fingerprint matches — the files defining the callee's name, their content
hashes and the root dependency manifests/lockfiles for definitions (a glob
import can gain a dependency symbol), the whole project revision for implementations
(implementors can change anywhere), (c) no *different* backend or server
version is live (a busy backend defers reuse instead of vouching for it), and
(d) for a resolved target, the recorded definition line still lies inside a
symbol of that name. Transient failures and "no result" are never cached.

Opportunistic queries (background, `ctx_callgraph`) never wait for a backend
busy with an interactive call, cap each request (5 s background, 2 s
interactive; also for the JetBrains bridge), and work against one absolute
deadline: a server start-up gets what is left, the request gets what is left
*after* it, and a server that never finished `initialize` is killed at once.
So they can neither hold a backend long nor overrun. Checking the live
backend's identity is a pure registry peek.

`GRAPH_ENGINE_VERSION` 6 forces existing property graphs — which may hold
unannotated, guessed `calls` edges — to rebuild once; until then an outdated
graph is never served (the index extractor bridges the gap). Version 7 does
the same for edges that bound a call across language families (a Rust call
landing on a same-named shell function): a callee now only resolves to
definitions the caller's language can actually call.

### 5. `semantic_mode` decides who may start a server

| Mode | Background enrichment | Interactive (`ctx_callgraph`) |
|---|---|---|
| `off` | never queries | never queries |
| `auto` (default) | only when a backend is already live for the project; never starts one | uses live backends only |
| `eager` | may start the project's language servers (trusted workspaces) | may start them (trusted workspaces) |

`ctx_refactor` keeps starting a server on demand, as before. Starting a
language server runs project code (build scripts, proc macros), so `eager`
applies only to a trusted workspace (`lean-ctx trust`) — whether it comes from
the global config, `LEAN_CTX_SEMANTIC_MODE`, or the project — and an untrusted
repository runs as `auto`; its own `.lean-ctx.toml` may lower the mode but not
raise it. The mode, its trust check, and backend selection are evaluated for
the project being processed, not for the process's working directory.

In `auto` the graph build usually runs in the daemon, while the live server
lives in the MCP process that `ctx_refactor` started it in. So the router
itself schedules a background pass whenever a backend is used in a process:
at most one per project every 5 minutes, only for a current graph (otherwise
retried on a use 30 s later), and only once the server is free and no tool
call has run in that process for 5 s — measured on a 2.4k-file crate, a pass
running alongside a tool call slowed it from ~3 s to ~30 s.

### 6. One backend per (project root, language)

The router keys backends by normalized project root and language, holds its
registry lock only for lookup/insert/evict, gives each backend its own lock
(single-flight start), evicts dead backends, shuts idle ones down after the
memory profile's TTL, and scrubs credential variables (`*_TOKEN`,
`*_SECRET`, `*_API_KEY`, …) from the server environment. `SSH_AUTH_SOCK` is
kept on purpose: a server running project code executes as the user and can
reach the agent anyway, while removing it breaks `cargo metadata` / `go list`
for private git dependencies — the scrub prevents reusable secrets from
leaking into server logs, it is not process isolation. Capabilities and server identity come from the `initialize`
handshake instead of being assumed; documents are opened once and updated
with `didChange`.

Which server serves a language is resolved per project. For TypeScript the
project's own `typescript` package decides: ≤ 6 ships `tsserver.js`, driven by
`typescript-language-server`; ≥ 7 (the native port) has no `tsserver.js`, and
its `tsc --lsp --stdio` is the server. Without a project TypeScript the
machine-wide install is used — `typescript-language-server` with the
`tsserver.js` installed beside it (passed as `tsserver.path`, since the server
does not look there itself), else a TypeScript ≥ 7 `tsc` on `PATH`. Versions
are read from `package.json`; nothing is executed to decide. npm's Windows
`.cmd` shims are not executables, so such a server starts as `node <the
package's bin entry>`, as the shim would. Doctor and the coverage surfaces
report the same resolution, including a binary configured as
`[lsp] <language> = "<path>"`.

### 7. Symbol relations: `implements`, `extends`, `references`

One generic pass (`core/semantic/relations.rs`) asks the backend about each
indexed symbol of the relevant kinds and lifts the answers to verified file
edges — budget-bounded, cached per site and project revision, pruned only
for declaring files whose every symbol got a definitive answer:

| Edge | Asked for | Backend request | When |
|---|---|---|---|
| `implements` (implementor → declaring file) | traits, interfaces | `textDocument/implementation` | background pass |
| `extends` (subtype → supertype file) | classes, structs, interfaces, traits | `prepareTypeHierarchy` + `typeHierarchy/supertypes` | background pass, where the backend offers a type hierarchy |
| `references` (user → declaring file) | every definition in one file | `textDocument/references` | on demand, for the file `ctx_impact` analyses |

A class's supertypes include the interfaces it implements; such a pair keeps
only the more specific `implements` edge. `references` costs one request per
symbol, so it runs where the question is exactly "who uses this?" — impact
analysis — rather than for the whole repository; its verified edges then
serve ranking and later analyses too. An empty or missing answer is never
definitive (a cold server reports nothing); a type hierarchy answer with a
type but no supertypes is.

`overrides` is evaluated and deliberately **not** stored as its own edge:
at file level every override is already an existing relation. A method
overriding a trait method lives in the `impl Trait for T` block that yields
the `implements` edge; a method overriding a base-class or interface method
lives in a class whose `extends`/`implements` edge links the same pair, and
an override of a grand-base method is reached through the chain of `extends`
edges. A separate `overrides` edge would duplicate those pairs and count
the same coupling twice in ranking — the double weighting this design rules
out. The per-method question ("which implementations override this?") is
answered on demand by `ctx_refactor action=implementations` on the method.

The repo map (`ctx_repomap`) uses the same call edges: those of a current
property graph (scope-bound, semantically verified, vetoed guesses removed),
else the caller-scope resolution — never the bare callee name, which used to
link a call to whichever same-named definition it saw first.

### 8. Editor bridge (VS Code, Cursor, Windsurf)

The lean-ctx editor extension serves the JetBrains plugin's loopback HTTP
protocol — its read-only navigation subset (`/health`, `/definition`,
`/declaration`, `/references`, `/implementations`, `/type_hierarchy`) — from
the editor's own language features, so whatever language extensions a user
has installed answer, and lean-ctx reuses one client for both. Each
workspace folder gets its own server on `127.0.0.1:0` behind a random
per-window token, announced by an owner-only JSON file in
`<data dir>/editor-bridges` (`lean-ctx editor-bridge dir`; the extension
asks the binary instead of re-deriving the data-dir layouts). lean-ctx
matches a bridge by its `project_root`, canonicalized on the Rust side,
rather than a hash both languages would have to compute identically.

A bridge answers only when the router has no backend running for the
language and no JetBrains IDE is attached; it never displaces a live server
and never starts anything. It is not a router backend, so `ctx_refactor`
edits never go to it. Requests are confined to the workspace folder — after
resolving symlinks and junctions; locations outside it travel as absolute
paths and read as external.

An announcement names an endpoint lean-ctx will trust, so both sides refuse
one another local user could have planted: the directory and each file must
belong to the current user and be writable by nobody else, and an
announcement must not be a symlink; the extension writes through an
exclusively created random temp file. `/health` must answer with the
announced editor, not merely any 2xx. The backend identity — the key of
cached answers — is the editor version plus a digest of the installed
extensions, so updating a language extension retires its cached answers.

A capped answer (`truncated` — an IDE limits large result sets) is never
complete: its files become edges, but it is not cached, settles nothing and
prunes nothing.

## Consequences

- Fewer false edges with or without a language server; more precise edges
  with one. Ranking counts a relationship once per kind at its strongest
  evidence instead of summing file- and symbol-level duplicates.
- Background cost is zero unless a backend is live (`auto`) and otherwise
  bounded per run (1000 definition lookups / 60 s — measured ~65 ms per warm
  rust-analyzer lookup on a 2.4k-file crate — and 100 lookups / 10 s each for
  `implements` and `extends`). A budget-limited run asks one site of every
  `(caller file, structural target)` group before a second site of any, and
  guessed edges before ambiguous ones, so each pass decides as many file
  edges as it can; answers accumulate in the cache across passes.
- `ctx_graph status` and `ctx_graph enrich` report verified / resolved /
  heuristic counts; `lean-ctx doctor` reports the mode and which servers can
  actually run (a rustup proxy without the component no longer counts).
- Coverage is reported per language (verified share of the caller files'
  `calls` edges, whether that language's server can run, and what the last
  backend started for it negotiated — e.g. "offers definition, references,
  implementations; no type hierarchy") in `ctx_graph status`, `lean-ctx
  doctor` and the dashboard's capability legend. The negotiated features are
  recorded at backend start in `<data dir>/semantic-backends.json`, so a
  doctor run sees what the MCP server's backend can do.
- `ctx_impact` propagates twice — over all edges and over edges that are not
  heuristic (each file pair weighted by its strongest edge of that class) —
  and lists files reachable only through name matches as `weak_files` ("name
  match only"), so a guess is never presented as a fact. Propagation is exact
  (a heavier path found later still propagates) and deterministic.

## Measured

`core::semantic::e2e_tests` (ignored; needs the servers installed) builds a
fixture per language with an ambiguous call (two same-named methods), a
decoy (a library call whose name also exists in the project), an interface
with two implementors and — in TypeScript — a subclass. It asserts the
ambiguous call is verified to the right file, the decoy is vetoed, no false
edge is written, a second pass is answered entirely from the cache,
`references` names the caller as a user of the target's file but not of the
decoy's, and `extends` names exactly the supertypes where the backend offers
a type hierarchy. The editor rows run a real editor window with the
extension; lean-ctx starts nothing (`auto`).

Measured on 2026-10-03 (Apple Silicon, macOS; small fixtures, so the cold
time is backend start plus first indexing, not a large-repository figure):

| Language | Backend | Ambiguous call | Decoy vetoed | `implements` | `references` | `extends` | False edges | Cold | Warm / query | Cached pass | RSS |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Rust | rust-analyzer 1.97.1 | verified | yes | yes | yes | no type hierarchy | 0 | 2.4 s | 1.0 ms | 0.1 ms | 592 MiB |
| TypeScript 7 | `tsc --lsp` (typescript-go 7.0.2) | verified | yes | yes | yes | no type hierarchy | 0 | 0.16 s | 0.3 ms | 0.1 ms | 49 MiB |
| TypeScript 5 | typescript-language-server + project TS 5.9.3 | verified | yes | yes | yes | no type hierarchy | 0 | 1.0 s | 0.8 ms | 0.1 ms | 134 MiB |
| Python | pylsp 1.15.0 | verified | yes | n/a | yes | n/a | 0 | 2.3 s | 1.2 ms | 0.1 ms | 212 MiB |
| Go | gopls v0.23.0 | verified | yes | yes | yes | yes | 0 | 0.35 s | 0.3 ms | 0.6 ms | 187 MiB |
| TypeScript | VS Code 1.140.0 editor bridge | verified | yes | yes | yes | no type hierarchy | 0 | 1.3 s | 11.4 ms | 0.3 ms | n/a |
| TypeScript | Cursor 1.128.0 editor bridge | verified | yes | yes | yes | no type hierarchy | 0 | 3.4 s | 7.3 ms | 0.3 ms | n/a |

"Cached pass" answers every question from `semantic_resolutions` without a
backend request. RSS is the server process tree (an editor is not a child of
the test). "No type hierarchy": the backend offers none for the language —
reported, not failed. The JetBrains path and the Rust side of the editor
bridge are covered by non-ignored tests against a local fake bridge (port
file / announcement, `/health`, `/definition`), since no IDE runs in CI; the
extension's server, confinement and announcement logic by its own tests.

## Not built

No bundled or auto-installed language servers, no external or LLM-based
resolution, no separate semantic graph, no synchronous whole-repo semantic
indexing at startup, no whole-repository `references` pass (see §7), and no
edits through the editor bridge (navigation only, §8).
