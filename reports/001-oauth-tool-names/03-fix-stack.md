# Fix Stack Report

## Possible changes at each layer

### Layer 1: Tool definitions (ri-tools)
**Change**: Rename all tools to PascalCase internally.
**Pros**: Simplest, fixes the issue everywhere.
**Cons**: Changes tool names for non-OAuth paths too. Makes tool names non-idiomatic Rust. The model sees PascalCase names even when using a regular API key.
**Verdict**: Not recommended as the sole fix. Tool names should be an internal concern; the OAuth compatibility should be handled at the provider layer.

### Layer 2: Anthropic provider (ri-ai/anthropic.rs) - tool name remapping
**Change**: When `is_oauth`, remap tool names to Claude Code canonical casing before sending, and remap back when receiving tool calls.
**Pros**: Exactly what pi does. Isolated to the OAuth path. Clean separation of concerns.
**Cons**: Requires maintaining a mapping table of CC tool names.
**Verdict**: This is the correct fix.

### Layer 3: Anthropic provider - error message improvement
**Change**: When a 400 error contains "credential is only authorized for use with Claude Code", add diagnostic context: log the tool names being sent and suggest checking tool name casing.
**Pros**: Even if the mapping table falls out of date, the error message will point to the cause.
**Cons**: Doesn't fix the issue, just makes it louder.
**Verdict**: Should be done in addition to Layer 2.

### Layer 4: Request observability
**Change**: Add trace-level logging of the full request body in the Anthropic provider.
**Pros**: Catches all future request-construction issues. Zero cost when not enabled.
**Cons**: None.
**Verdict**: Should be done.

## Recommended changes (in order)

### 1. Make the issue obvious (Layer 3)
In `parse_error_body`, when the error message contains "credential is only authorized for use with Claude Code", log a warning with the tool names being sent. This ensures that even if the remapping fails or a new tool conflicts, the issue is immediately diagnosable.

```rust
// In parse_error_body or the caller
if error_message.contains("only authorized for use with Claude Code") {
    warn!("OAuth credential rejected. This usually means tool names don't match Claude Code's expected casing. Sent tools: {:?}", tool_names);
}
```

### 2. Fix the issue (Layer 2)
Add a tool name remapping table in the Anthropic provider, matching pi's approach:

```rust
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read", "Write", "Edit", "Bash", "Grep", "Glob",
    "AskUserQuestion", "EnterPlanMode", "ExitPlanMode",
    "KillShell", "NotebookEdit", "Skill", "Task",
    "TaskOutput", "TodoWrite", "WebFetch", "WebSearch",
];

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS.iter()
        .find(|cc| cc.eq_ignore_ascii_case(name))
        .map(|cc| cc.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, tools: &[ToolSchema]) -> String {
    tools.iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .map(|t| t.name.clone())
        .unwrap_or_else(|| name.to_string())
}
```

Apply `to_claude_code_name` in `convert_tool` when `is_oauth`.
Apply `from_claude_code_name` when parsing tool call responses in the SSE stream.

### 3. Add observability (Layer 4)
Add `tracing::debug!` call with the request body before sending. This is a one-liner.
