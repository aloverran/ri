# Test Harness Deficiencies Report

## Current state

ri has no test infrastructure. No unit tests, no integration tests, no test harness. The only way to detect this issue was to run the full binary interactively and observe the error.

## Deficiencies discovered during investigation

### 1. No request-level observability

The Anthropic provider has no way to inspect what it's about to send. During debugging, I had to add temporary `eprintln!` calls to see the actual request body and headers. 

**Improvement**: Add a `--dump-request` or `RUST_LOG=ri_ai=trace` level that logs the full HTTP request (headers + body) before sending. This is not a test - it's a diagnostic tool that makes future issues self-evident.

### 2. No way to run the agent non-interactively with exit code feedback

The `--mode print --prompt "..."` flag exists and works, which is good. This is the right shape for a test harness entry point. However, the error output goes through color-eyre's formatting which is hard to parse programmatically.

**Improvement**: `--mode print` should have a `--output json` mode that outputs structured errors (it partially exists but isn't wired to error paths).

### 3. No dry-run / request-only mode

There's no way to see what ri *would* send to the API without actually sending it. This would have immediately revealed the tool name issue.

**Improvement**: A `--dry-run` flag that constructs the full request body (with all tools, system prompt, etc.) and dumps it as JSON to stdout without making the API call. This is a cheap diagnostic that catches an entire class of request-construction bugs.

### 4. The error message doesn't help locate the problem

The error `API error (400): invalid_request_error: This credential is only authorized for use with Claude Code and cannot be used for other API requests.` passes through Anthropic's error verbatim, which is misleading. The actual problem is tool name casing, but the error says "credential not authorized."

**Improvement**: When an OAuth request gets a 400, the provider should log the request body (at debug level) so the user can compare against what Claude Code sends. Even better: detect common OAuth-specific failures (like this one) and add a hint.

### 5. No curl-equivalent output

The fastest debugging path was constructing curl commands to bisect the issue. ri should be able to output the equivalent curl command for any request it makes.

**Improvement**: A `--trace-curl` flag or `RI_TRACE_CURL=1` env var that prints the equivalent curl command before each API call. This is zero-cost when disabled and invaluable when debugging.

## Summary

The core deficiency is **request observability**. The issue was entirely in the HTTP request construction, but ri provided no way to see what it was sending. The fixes are all about making the request pipeline transparent:

1. `--dry-run` to dump request without sending
2. Trace-level logging of full requests
3. `--trace-curl` for quick reproduction outside ri
