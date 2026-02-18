# TUI Architecture: Scrollback + Full-Screen Viewport

## Overview

The TUI uses ratatui's `Viewport::Inline(terminal_height)` to create a full-screen
viewport that stays in the main screen buffer (no alternate screen). This gives us
two rendering surfaces:

1. **The Viewport** — a full-screen ratatui canvas for panels, status bars, content
   tail, and all interactive UI. Rendered via `Terminal::draw()`.
2. **Terminal Scrollback** — completed content blocks are pushed here via
   `Terminal::insert_before()`. Users scroll up in their terminal to browse history.

The in-memory `Vec<ContentBlock>` is the single source of truth. The viewport
renders the tail of this vec. Scrollback is a backup for scroll-up access.

## Data Model

```
ContentBlock
  - id: BlockId
  - kind: Text | Thinking | ToolCall | ToolResult | Usage | Error
  - content: the raw data (markdown text, JSON, etc.)
  - state: Collapsed | Expanded | Hidden
  - rendered: Option<RenderedBlock>  // cached ratatui render

RenderedBlock
  - lines: Vec<Line<'static>>       // ratatui styled lines
  - height: u16                      // post-wrap height at last render width
  - width: u16                       // terminal width this was rendered at
```

Content blocks are created from `AgentEvent`s. Each event type maps to a block
kind. During streaming, the current block is "in-progress" and rendered live in
the viewport. When complete, it's finalized, emitted to scrollback, and the next
block begins.

## Rendering Pipeline

### Normal Frame (viewport draw)

```
[layout]
+-----------------------------------------+
|                                         |
|   content tail (recent blocks)          |  <- scrollable within viewport
|                                         |
|   in-progress block (streaming)         |
|                                         |
+-----------------------------------------+
| status bar: model, tokens, phase        |
+-----------------------------------------+
```

With a side panel open:

```
[layout]
+-------------------+---------------------+
|                   |                     |
|   panel           |   content tail      |
|   (file browser,  |                     |
|    settings,      |   in-progress       |
|    help)          |                     |
|                   |                     |
+-------------------+---------------------+
| status bar                              |
+-----------------------------------------+
```

The viewport always renders the last N lines of content that fit in the
available content area. This is standard ratatui widget rendering — Paragraph,
Block, Layout, etc.

### Content Block Emission (insert_before)

When a content block completes:

```rust
// 1. Render the block to ratatui styled lines
let block = finalize_block(events);
let lines = render_block(&block, terminal_width);
let height = lines.len() as u16;

// 2. Wrap in sync output for atomic terminal update
write!(stdout, "\x1b[?2026h")?;  // sync start

// 3. Push to terminal scrollback
terminal.insert_before(height, |buf| {
    let text = ratatui::text::Text::from(lines.clone());
    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .render(buf.area, buf);
});

// 4. Redraw viewport (content tail shifts since block is now "emitted")
terminal.draw(|frame| { render_viewport(frame, &state); })?;

write!(stdout, "\x1b[?2026l")?;  // sync end
stdout.flush()?;
```

The sync output (DEC 2026) batches the insert + viewport redraw into a single
atomic terminal update. The user sees the block appear in scrollback and the
viewport update simultaneously — no flicker.

### Full Re-render (scrollback mutation)

When scrollback content changes (expand/collapse, hide/show, terminal resize):

```rust
write!(stdout, "\x1b[?2026h")?;   // sync start
write!(stdout, "\x1b[3J")?;       // clear scrollback buffer

// CRITICAL: use terminal.clear(), not manual \x1b[2J.
// terminal.clear() resets ratatui's back buffer so the next draw()
// does a full redraw. Without this, ratatui's diff will be against
// stale state and produce broken output.
terminal.clear()?;

// Re-emit all visible blocks to scrollback
for block in &blocks {
    if block.state == Hidden { continue; }
    let lines = render_block(block, terminal_width);
    let height = lines.len() as u16;
    terminal.insert_before(height, |buf| {
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .render(buf.area, buf);
    });
}

// Full redraw of viewport (back buffer was reset by clear())
terminal.draw(|frame| { render_viewport(frame, &state); })?;

write!(stdout, "\x1b[?2026l")?;   // sync end
stdout.flush()?;
```

This is expensive but infrequent. Triggers:
- User expands/collapses a tool result
- User hides/shows thinking blocks
- Terminal resize (width change invalidates all wrapped heights)

**Terminal.app warning:** macOS's default Terminal.app does not support
DEC 2026 synchronized output. On Terminal.app, the clear + re-emit will
produce a visible flash (blank screen -> content reappearing). This is
acceptable for a developer tool — developers using Rust CLI tools
overwhelmingly use iTerm2, Kitty, Ghostty, Alacritty, or WezTerm, all
of which support DEC 2026. If Terminal.app support is important, consider
disabling scrollback mutation entirely (blocks are immutable in
scrollback, only interactive in the viewport).

## How insert_before Works with Full-Height Viewport

With `scrolling-regions` enabled and `viewport_height == screen_height`,
ratatui uses this algorithm:

1. Draw the first line of the insert buffer onto row 0 of the screen
2. Set scroll region to just row 0: `\x1b[1;1r`
3. Scroll up: `\x1b[1S` (pushes row 0 into terminal scrollback)
4. Reset scroll region: `\x1b[r`
5. Draw the next line onto the now-empty row 0
6. Repeat until all lines are emitted
7. Restore row 0 with the viewport's actual top-row content

This is O(n) operations for n lines. Each operation is a few escape sequences
plus one row of cell data. Wrapped in sync output, the intermediate states
are invisible.

**Performance:** A 200-line content block generates ~200 scroll cycles. At
terminal I/O speeds, this is ~10-20ms total. Well within acceptable bounds for
a "block completed" event that happens every few seconds at most.

## Sync Output (DEC 2026)

Ratatui does NOT handle synchronized output. You must manage it yourself.

```rust
fn sync_render<F>(stdout: &mut impl Write, f: F) -> io::Result<()>
where F: FnOnce() -> io::Result<()>
{
    write!(stdout, "\x1b[?2026h")?;
    let result = f();
    write!(stdout, "\x1b[?2026l")?;
    stdout.flush()?;
    result
}
```

Wrap ALL visible-state-changing operations:
- `insert_before` + `draw` pairs
- Full re-renders
- NOT needed for pure `draw` calls if the diff is small (ratatui's diff is
  already minimal, and the viewport position doesn't change)

Terminal support: iTerm2, Kitty, Ghostty, Alacritty, WezTerm, Windows
Terminal all support DEC 2026. Most modern terminals do. Terminals that
don't support it silently ignore the escape sequences (it's a no-op, not
an error).

## Content Block Rendering

Each block is rendered to ratatui `Line`s independently. Use ratatui's
widget system for rich formatting:

```rust
fn render_block(block: &ContentBlock, width: u16) -> Vec<Line<'static>> {
    match &block.kind {
        BlockKind::Text(md) => {
            // Use tui-markdown or manual spans for markdown rendering
            tui_markdown::from_str(md).lines
        }
        BlockKind::Thinking(text) => {
            // Dim styled lines
            text.lines().map(|l|
                Line::styled(l.to_string(), Style::default().dim())
            ).collect()
        }
        BlockKind::ToolCall { name, .. } => {
            vec![Line::from(vec![
                Span::styled("tool: ", Style::default().fg(Color::Yellow)),
                Span::raw(name.clone()),
            ])]
        }
        BlockKind::ToolResult { output, is_error, .. } => {
            let style = if *is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().dim()
            };
            // Truncate or collapse based on state
            // ...
        }
        // ...
    }
}
```

### Height Calculation

Use `Paragraph::line_count(width)` to determine the wrapped height before
calling `insert_before`:

```rust
let text = Text::from(lines.clone());
let para = Paragraph::new(text).wrap(Wrap { trim: false });
let height = para.line_count(width) as u16;
```

This avoids the "render to oversized buffer and trim" approach, which
wastes memory (a 1000x200 buffer is ~7MB of Cell structs).

## Terminal Resize

On resize:
1. ratatui's `autoresize()` (called by `draw()`) handles viewport resizing
2. All cached `RenderedBlock` heights are invalidated (width changed)
3. Trigger a full re-render (clear scrollback + re-emit at new width)

Resize is inherently a "mutation" event that requires full re-render.
This is fine — resizes are infrequent.

## Lifecycle

```
Session Start
  |
  v
Create Terminal with Viewport::Inline(term_height)
  |
  v
[Input Loop]  <----------------------------+
  |                                         |
  v                                         |
User submits prompt                         |
  |                                         |
  v                                         |
[Agent Loop]  <-----------+                 |
  |                       |                 |
  v                       |                 |
Stream events             |                 |
  |                       |                 |
  +-> Thinking deltas     |                 |
  |   (render in viewport)|                 |
  |                       |                 |
  +-> Text deltas         |                 |
  |   (progressive md     |                 |
  |    in viewport)       |                 |
  |                       |                 |
  +-> Block complete      |                 |
  |   (insert_before +    |                 |
  |    viewport redraw)   |                 |
  |                       |                 |
  +-> Tool call           |                 |
  |   (emit tool block,   |                 |
  |    execute, emit       |                 |
  |    result block)       |                 |
  |   loop ---------------+                 |
  |                                         |
  v                                         |
Agent done                                  |
  |                                         |
  v                                         |
Emit final blocks                           |
  |                                         |
  +-> back to input -----------------------+
```

## Key Invariants

1. **The viewport is always full-height.** It owns the entire visible screen.
   Scrollback is invisible unless the user scrolls up in their terminal.

2. **The in-memory Vec is the source of truth.** Scrollback is a projection.
   Re-rendering scrollback from the Vec must produce identical output.

3. **Blocks are immutable once emitted.** A block's content doesn't change
   after emission. State changes (collapse/expand) trigger full re-render.

4. **The viewport renders the content tail.** The last N lines of rendered
   blocks that fit in the content area. This is recalculated on every
   `draw()` call.

5. **Sync output wraps all compound operations.** Any sequence that changes
   both scrollback and viewport must be atomic.

## What This Replaces

- **DiffRenderer** (`tui.rs`): Goes away entirely. Ratatui's double-buffer
  diffing replaces the manual ANSI diff logic.

- **TuiRenderer** (`interactive.rs`): Refactored into the new architecture.
  The progressive markdown streaming, phase tracking, and event handling
  stay, but rendering goes through the new pipeline.

## Considerations

### Terminal Scrollback Limits

Terminal emulators have configurable scrollback limits (typically 10k-100k
lines). Since we keep all content blocks in memory and can re-emit
scrollback at any time, this is not a data-loss concern. The worst case
is that the uppermost content blocks get truncated in the terminal's
scrollback view — the terminal simply drops the oldest lines when its
buffer fills. This is transparent to ratatui (it doesn't track scrollback
content) and causes no state corruption or rendering issues.

For very long sessions (50k+ lines), a full re-emit takes proportionally
longer (O(n) scroll operations). Consider capping re-emit to the last N
lines that fit within a reasonable scrollback budget, or only re-emitting
the last ~10k lines and accepting that extremely old blocks won't be
scrollable.

### Scrollback Width Mismatch

When a side panel is open, the viewport renders content at reduced width.
Scrollback blocks are always at full terminal width. This visual
discontinuity is minor and expected — scrollback is the "archival" view.

### Input Widget (replacing reedline)

reedline is replaced with a ratatui-native text editor widget rendered
inside the viewport. Two viable options, both confirmed to compile with
ratatui 0.30:

**`tui-textarea` (v0.7)** — Simple multi-line input with Emacs-like
keybindings. Supports undo/redo, regex search, line numbers, cursor
highlighting. Implements `Widget` directly. Lightweight — no syntax
highlighting or modal editing. Good fit for a prompt input field.

```toml
tui-textarea = { version = "0.7", default-features = false, features = ["crossterm"] }
```

```rust
use tui_textarea::TextArea;
let mut textarea = TextArea::default();
// In event loop: textarea.input(key_event);
// In draw:       frame.render_widget(&textarea, input_area);
// Get text:      textarea.lines()
```

**`edtui` (v0.11)** — Vim-inspired modal editor (Normal/Insert/Visual).
Syntax highlighting via syntect, system clipboard via arboard, mouse
support, line wrapping. Heavier (~syntect + onig + image dependencies).
Better for editing code blocks or longer text.

```toml
edtui = "0.11"
```

```rust
use edtui::{EditorState, EditorView, EditorEventHandler};
let mut state = EditorState::default();
let mut handler = EditorEventHandler::default();
// In event loop: handler.on_key_event(key_event, &mut state);
// In draw:       frame.render_widget(EditorView::new(&mut state), input_area);
```

**Recommendation:** `tui-textarea` for prompt input. It's simpler, has
fewer dependencies, and the Emacs-like keybindings match what users
expect from a CLI prompt. The viewport layout would be:

```
[layout]
+-----------------------------------------+
|   content tail / in-progress            |
+-----------------------------------------+
| > textarea input                        |
+-----------------------------------------+
| status bar                              |
+-----------------------------------------+
```

The textarea submits on Enter (configurable), and the input text becomes
the next user message in the agent loop.

### tmux Compatibility

tmux has a known quirk: full screen clear + immediate scrolling can produce
garbage in scrollback. The `scrolling-regions` feature in ratatui was
partly motivated by this. With sync output + scrolling regions, tmux
behavior should be clean, but test it.
