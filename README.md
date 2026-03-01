# Ri

BYO-coding agent. Ri is a set of Rust libraries that let you build your own Claude-code style coding agent. The idea is that instead of writing plugins and extensions to a black-box core, each person builds their own coding agent from a set of pieces.

In Ri, your coding agent is just a Rust project that uses the `ri` and `ri-tools` packages. On top, you write custom Rust scripts, terminal UI, or LLM tools, or pull from existing `ri` packages uploaded by other users to crates.io. Run your coding agent with `cargo run --release` or compile it to a binary with `cargo build --release`.

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

Ri provides a file-based storage for session history. The data model has two primitives: **messages** (pure content blobs) and **contexts** (ordered selections of messages -- what the LLM sees). On top of those, a **step** records a context at a point in the history DAG: it's a context + provenance (parent links and metadata), like a git commit is a tree + parents.

A message is just text with a role (user, assistant, system). It carries no information about which LLM call produced it or what context it was part of. Messages live in a shared pool, referenced by globally unique IDs.

A context is an ordered list of message references. Resolved against the pool, it gives you `Vec<Message>` -- exactly what you hand to the LLM. A session is like a git branch -- a named pointer to the latest step.

In the simplest of chats, the history is a linear chain of steps, each adding the latest messages to the context. But because steps form a DAG, advanced workflows are natural:

- **Branching**: Fork the history and explore different approaches, then merge.
- **Compaction**: Summarize old messages into a new message, create a step with the summary replacing the originals.
- **Cross-session**: Pull messages from another session into this one. The globally unique IDs make it work.
- **Fan-out**: Send the same context to different models, compare results.
- **Editing history**: Create a new step with a modified context. The old steps remain.

The point of this architecture is to enable much more interesting agent session structures, even abandoning the concept of sessions at all.

But of course, you can work with Ri in the standard 'chatbot' way that you'd expect. In which case, the storage just becomes a linear chain of steps with growing contexts.

_This file was written by a human._
