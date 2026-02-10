# ri-tools

Built-in tool implementations: `bash`, `read`, `write`, `edit`.

Each is a function that returns a `ToolDef` (from ri-core). `all_tools()` returns the full set.

## Tools

**bash** — Runs a shell command via `sh -c`. Captures stdout/stderr, returns exit code. Configurable timeout (default 120s). Respects cancellation token. Output is truncated at 2000 lines or 50KB.

**read** — Reads a file's contents with optional `offset` (1-indexed line) and `limit` (default 2000 lines). Returns line-numbered output.

**write** — Writes content to a file. Creates parent directories if needed.

**edit** — Find-and-replace in a file. Requires the old text to appear exactly once (errors on zero or multiple matches). Replaces the first occurrence.

## Depends on

ri-core. External: tokio, serde_json, tokio-util.
