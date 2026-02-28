# ri-tools

Building blocks for agent applications: coding tools, prompt templates, and context discovery.

## Coding tools

Built-in tool implementations: `bash`, `read`, `write`, `edit`. Each implements the `Tool` trait from ri. `all_tools()` returns the full set.

**bash** -- Runs a shell command via `sh -c` in a process group. Captures stdout/stderr, returns exit code. Configurable timeout (default 120s). Respects cancellation token. Output is truncated at 2000 lines or 50KB.

**read** -- Reads a file's contents with optional `offset` (1-indexed line) and `limit` (default 2000 lines). Returns line-numbered output.

**write** -- Writes content to a file. Creates parent directories if needed.

**edit** -- Find-and-replace in a file. Requires the old text to appear exactly once (errors on zero or multiple matches).

## Prompt templates (`prompts`)

Load `.md` template files from any directory. Templates support variable substitution:
- `$1`, `$2`, ... -- positional args
- `$@` and `$ARGUMENTS` -- all args joined
- `${@:N}` -- args from Nth onwards (1-indexed)
- `${@:N:L}` -- L args starting from Nth

Templates can have YAML frontmatter with a `description` field. Substitution is single-pass (no re-expansion). `parse_command()` parses `/name args...` invocations, `expand_prompt()` matches against loaded templates.

## Context and resources (`resources`)

Shared context discovery used by both ri-cli and ri-web:

**Context files** -- Discovers `AGENTS.md` / `CLAUDE.md` by walking up from the working directory, stopping at `.git` boundaries. Also checks `.agents/` subdirectories and the global config (`~/.config/agents/`). Supports recursive `{{include:path}}` directives with cycle detection and boundary enforcement.

**Settings** -- Loads `~/.config/agents/settings.json` for `defaultModel` and `defaultThinking` preferences.

**System prompt** -- `BASE_SYSTEM_PROMPT` constant, `get_environment_system_prompt()` for platform/date/session info, `format_context_files()` for discovered context.

## Depends on

ri. External: tokio, serde_json, tokio-util, command-group, tracing, chrono, os_info.
