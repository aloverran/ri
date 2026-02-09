# Root Cause Report: OAuth Credential Rejection (400)

## Error
```
API error (400): invalid_request_error: This credential is only authorized for use with Claude Code and cannot be used for other API requests.
```

## Reproduction
```sh
cargo run -- --mode print --prompt "say hello"
```
Requires: OAuth token in `~/.ri/auth.json` (prefix `sk-ant-oat`), no `ANTHROPIC_API_KEY` env var.

## Root Cause

**The Anthropic API server-side validates that OAuth token (`sk-ant-oat`) requests match Claude Code's expected request format, specifically including tool name casing.**

ri sends tool names in lowercase: `bash`, `read`, `write`, `edit`, `grep`, `find`, `ls`.

Claude Code uses PascalCase: `Bash`, `Read`, `Write`, `Edit`, `Grep`, `Glob`, etc.

The API performs a case-insensitive match of tool names against Claude Code's known tool list. If a tool name matches a known CC tool but uses incorrect casing, the entire request is rejected with the credential error.

### Verified behavior (via curl with the same token):

| Tool name sent | Matches CC tool? | Casing correct? | Result |
|---|---|---|---|
| `Bash` | Yes | Yes | OK |
| `bash` | Yes (case-insensitive) | No | REJECTED |
| `Read` | Yes | Yes | OK |
| `read` | Yes (case-insensitive) | No | REJECTED |
| `find` | No (CC uses `Glob`) | N/A | OK |
| `ls` | No CC equivalent | N/A | OK |
| `MyCustomTool` | No | N/A | OK |
| `my_read_tool` | No | N/A | OK |

### Affected tools (5 of 7):
- `bash` -> must be `Bash`
- `read` -> must be `Read`
- `write` -> must be `Write`
- `edit` -> must be `Edit`
- `grep` -> must be `Grep`

### Unaffected tools (2 of 7):
- `find` - no case-insensitive CC match (CC has `Glob`, not `Find`)
- `ls` - no CC equivalent

## Why pi doesn't have this issue

pi's Anthropic provider (`pi-ai/dist/providers/anthropic.js`) maintains a lookup table of Claude Code tool names and remaps all tool names to CC canonical casing when `isOAuthToken` is true:

```js
const claudeCodeTools = ["Read", "Write", "Edit", "Bash", "Grep", "Glob", ...];
const ccToolLookup = new Map(claudeCodeTools.map((t) => [t.toLowerCase(), t]));
const toClaudeCodeName = (name) => ccToolLookup.get(name.toLowerCase()) ?? name;
```

It also remaps tool names back from CC casing when receiving tool call responses from the model.

## Layers of the issue

1. **Anthropic API (server)**: Enforces tool name validation for OAuth tokens. The error message is misleading - it says "credential is only authorized for Claude Code" when the actual issue is tool name casing mismatch. A better error would mention the specific validation that failed.

2. **ri's Anthropic provider** (`crates/ri-ai/src/anthropic.rs`): Already detects OAuth tokens and adds CC-compatible headers/system prompt, but does not remap tool names.

3. **ri's tool definitions** (`crates/ri-tools/src/`): Define tools with lowercase names (idiomatic Rust convention), which conflicts with CC's PascalCase convention when used with OAuth tokens.
