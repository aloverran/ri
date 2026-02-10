# ri-cli

The `ri` binary. Wires together ri-store, ri-core, ri-ai, and ri-tools into a working coding agent.

## Run modes

**interactive** (default) — REPL. Reads user input, runs the agent loop, streams output to the terminal. Persists all messages to session files via `SessionFiling`. Supports `/login` (Anthropic OAuth, Google OAuth), `/help`, `/quit`.

**print** (`--mode print`) — Single-shot. Requires `--prompt`. Runs one agent loop and prints the result to stdout. No session persistence. With `--output json` or `--mode json`, emits JSONL events instead of plain text.

**rpc** (`--mode rpc`) — JSON-RPC over stdio. Reads `{"type": "prompt", "message": "..."}` commands from stdin, runs the agent loop, emits JSONL events to stdout. Used for programmatic integration.

## Configuration

**Provider selection** — `--provider` flag or `defaultProvider` in `~/.ri/settings.json`. Supports `anthropic`, `google-gemini-cli`, `google-antigravity`.

**Model selection** — `--model` flag or `defaultModel` in settings. Hardcoded defaults: Sonnet 4 (Anthropic), Gemini 2.5 Pro (Gemini CLI), Gemini 3 Pro (Antigravity). Custom models via `~/.ri/models.json`.

**Auth** — API keys from `models.json`, env vars (`ANTHROPIC_API_KEY`), or OAuth tokens stored in `~/.ri/auth.json`. Interactive mode supports `/login` to initiate OAuth flows.

**Resources** — Discovers `AGENTS.md` / `CLAUDE.md` context files by walking up from cwd and injects them into the system prompt. Also discovers skills (`<dir>/.ri/skills/<name>/SKILL.md`) and prompt templates (`<dir>/.ri/prompts/*.md`) for future use (skill injection requires explicit activation; prompts are discovered but not yet wired up).

## Depends on

ri-store, ri-core, ri-ai, ri-tools. External: clap, color-eyre, tokio, serde, dirs, glob, tracing-subscriber.
