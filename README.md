# Ri

BYO-coding agent. Ri is a set of Rust libraries that let you build your own Claude-code style coding agent. The idea is that instead of writing plugins and extensions to a black-box core, each person builds their own coding agent from a set of pieces.

In Ri, your coding agent is just a Rust project that uses the `ri` and `ri-cli` packages. On top, you write custom Rust scripts,  terminal UI, or LLM tools, or pull from existing `ri` packages uploaded by other users to crates.io. Run your coding agent with `cargo run --release` or compile it to a binary with `cargo build --release`.

Ri is heavily inspired by the `pi` project. Many thanks to `badlogic`.

# Installation

To test it out, you can install the example ri agent with `cargo install ri`. However, the expected workflow is to create a new project and run that:

```rust
mkdir my-ri && cd my-ri
cargo init
```


# Philosophy:

Pi has the right idea by keep a slim core, but when working with it I found that I constantly rubbed up against the friction of the extension interface. The UI isn't quite modifiable enough, I can't change how commands work, or how messages are stored. 

Ri takes an even simpler approach: Why not make your configuration just normal Rust code? Ri has a small core that manages interacting with the LLM APIs, storing messages on disk, etc. Then you simply build the UI and tooling on top of it that you prefer.

And since the configuration is just Rust code, common configuration (like a Claude Code style TUI) can simply be packages on `crates.io` that you can pull in. 

Ri is heavily inspired by `pi-agent`. It has a slim core focused on 4 tools: `read`, `edit`, `write`, and `bash`. However, there are a few big differences:

## No Extensions

Ri does not come with a way to install 'extensions'. If you want to extend your ri-agent, just have it write Rust to modify itself!

It comes with an included prompt called /self-modify that reads the ri documentation and source and creates a new session for modifying ri. If you want the ability to call sub-agents, you might say:

`/self-modify Add a subagent tool in the style of Claude Code.`

Then restart ri.

## Message Storage

Like Pi, of the things Ri provides is a file-based storage for session messages. However, the way Ri approaches storage is more like a database of messages with parent pointers. It treats the 'message' as the fundamental building block of an LLM session. Each message is just text, and is either authored (user messages, tool results) or generated (the result of an LLM call). Generated messages have `provenance`, that is they store pointers to the list of messages that defined the context for that call. 

In the simplest of chats, this is simply stored as a list of messages, however when you move to more advanced workflows (forking, editing history, compaction, summarization, cross-session communication) the message history becomes a directed graph of messages, each pointing at the set of messages it came from.

This is very powerful because it means that editing the message history is a first-class operation. Here are some operations that are simple with Ri that would be complex with other tools:
- Remove all tool calls from history.
- Fan out 5 LLM turns with the current message history, then summarize the results into a single message and add it to the current history.
- Query Ri: Which sessions touched this file?

The message history forms an immutable DAG of LLM results, each time you take a turn, a new message is appended to the bottom.

The point of this architecture is to enable much more interesting agent session structures, even abandoning the concept of sessions at all.

But of course, you can work with Ri in the standard 'chatbot' way that you'd expect. In which case, the storage just becomes a linear list of messages, as you'd expect.

_This file was written by a human._

