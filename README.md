# Ri

BYO-coding agent. Ri is a set of Rust libraries that let you build your own Claude-code style coding agent. The idea is that instead of writing plugins and extensions to a black-box core, each person builds their own coding agent from a set of pieces.

In Ri, your coding agent is just a Rust project that uses the `ri` crates. On top, you write your own terminal application or web UI, and ri provides the data storage and building blocks.

_Ri is heavily inspired by the `pi` project. Many thanks to `badlogic`._

# Installation

Ri provides an example to base your build on. It implements both a TUI and Web UI. To test it out, you can install the example ri agent with `cargo install ri`. However, the expected workflow is to create a new project and run that:

```bash
git clone https://github.com/aloverran/my-ri ri-example
cd ri-example/ri-tui && cargo run --release 
```

From here you can simply instruct Ri to modify itself however you like. There are a bunch of building blocks in the `ri-kit` crate.

If you want to start your Ri build from the example, we recommending forking the example repo. Or if you want to start fresh, just use the ri crates directly!

# Philosophy:

Pi has the right idea by keep a slim core, but when working with it I found that I constantly rubbed up against the friction of the extension interface. The UI isn't quite modifiable enough, I can't change how commands work, or how messages are stored.

Ri takes an even simpler approach: Why not make your configuration just normal Rust code? Ri has a small core that manages interacting with the LLM APIs, storing messages on disk, etc. Then you simply build the UI and tooling on top of it that you prefer.

And since the configuration is just Rust code, common configuration (like a Claude Code style TUI) can simply be packages on `crates.io` that you can pull in.

Like Pi, it has a slim core focused on 4 tools: `read`, `edit`, `write`, and `bash`. However, there are a few big differences:

## No Extensions

Ri does not come with a way to install 'extensions'. If you want to extend your ri, just have it modify itself!

## Message Storage

Ri provides a file-based storage for session history. The data model has two primitives: **messages** (immutable content blobs) and **contexts** (immutable objects -- an ordered message list, parent links, and metadata). Contexts form a DAG through their parents. A session is just a pointer to a context.

A message is just text with a role (user, assistant, system). It carries no information about which LLM call produced it or what context it was part of. Messages live in a shared pool, referenced by globally unique IDs.

A context's message list, resolved against the pool, gives you `Vec<Message>` -- exactly what you hand to the LLM. The system is messages, contexts, and the algebra on them -- nothing else.

In the simplest of chats, the history is a linear chain of contexts, each adding the latest messages to the list. But because contexts form a DAG, advanced workflows are natural:

- **Branching**: Fork the history and explore different approaches, then merge.
- **Compaction**: Summarize old messages into a new message, create a context with the summary replacing the originals.
- **Cross-session**: Pull messages from another session into this one. The globally unique IDs make it work.
- **Fan-out**: Send the same context to different models, compare results.
- **Editing history**: Create a new context with a modified message list. The old contexts remain.

The point of this architecture is to enable much more interesting agent session structures, even abandoning the concept of sessions at all.

But of course, you can work with Ri in the standard 'chatbot' way that you'd expect. In which case, the storage just becomes a linear chain of contexts with growing message lists.

_This file was written by a human._
