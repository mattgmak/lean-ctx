# lean-ctx — Replace Mode for Pi

This project uses the **pi-lean-ctx** extension in **Replace mode**. Native Pi builtins
(read/bash/grep/find/ls) are **suppressed**. You MUST use `ctx_*` tools exclusively.

## Tool mapping (MANDATORY)

| Use (ctx_*) | Instead of (suppressed) | Why |
|-------------|------------------------|-----|
| `ctx_read` | `read`, `cat`/`head`/`tail` | Cached + compressed; unchanged re-reads cost ~13 tokens |
| `ctx_shell` | `bash` | Shell output compressed via 95+ patterns |
| `ctx_search` | `grep` | Compact, ranked matches |
| `ctx_glob` | `find` | Compressed, .gitignore-aware file matching |
| `ctx_tree` | `ls` | Compact directory maps |

Do NOT attempt native `read`, `bash`, `grep`, `find`, or `ls` — they are not available.

## Editing

- Use `ctx_read(mode="anchored")` for files you will edit, then `ctx_patch` (line+hash anchors).
- Pi's native `edit`/`write` remain fully available for file modifications.
- For line ranges: `offset`/`limit` or `mode=lines:N-M` — all cached through the bridge.

## MCP bridge

The embedded bridge holds a persistent session cache. Use `/lean-ctx` to verify it reports
`connected`. To force the one-shot CLI path: `LEAN_CTX_PI_ENABLE_MCP=0`.
