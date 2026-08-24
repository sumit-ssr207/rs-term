# rs-term

A **node-based terminal manager**, written in Rust. Your terminals and notes
live as draggable cards on infinite, tabbed pan/zoom canvases instead of stacked
tabs — a native Rust reimagining of the core idea behind
[nodeterm](https://github.com/eneskirca/nodeterm), built with
[`egui`](https://github.com/emilk/egui). No Electron, single ~6 MB binary.

## ⚠️ Installing the app (unsigned build — read this first)

> [!IMPORTANT]
> This build is **not signed with a paid Apple Developer ID and not notarized**,
> so on first launch macOS Gatekeeper will block it with a message like
> *"can't be opened / unidentified developer / is damaged."* **This is expected**
> — the app is fine. After unzipping, do **one** of the following (only needed once):
>
> - **Right-click `rs-term.app` → Open**, then click **Open** in the dialog, **or**
> - **Run in Terminal:**
>   ```bash
>   xattr -cr /Applications/rs-term.app
>   ```
>   (clears the quarantine flag macOS adds to downloaded apps)

## Features

- **tmux-backed terminals** — every terminal node is a persistent tmux session
  on a private tmux server (`-L rs-term`). Sessions **survive app restarts**:
  quit and relaunch and your shells (and anything running in them) are still
  there, re-attached automatically. Needs `tmux` on `PATH`/Homebrew; without it
  the app falls back to a plain shell (no persistence) and says so.
- **Auto-titles** — each node's title live-summarises what it's doing. Terminals
  show the current foreground command + directory (e.g. `vim · src`, `cargo ·
  rust_term`, or just the dir when idle), polled from tmux. Notes summarise their
  first line.
- **Tabbed canvases** — multiple independent canvases across the top. Add with
  `＋`, switch by clicking, close with `✕` (closing a canvas kills its sessions).
- **Infinite canvas** — pan by dragging empty space; **scroll to zoom** toward
  the cursor; **hold ⌥ (Option) and two-finger scroll to pan**. Subtle grid.
- **Zoom controls** — `－ / ＋ / 100% / Fit` buttons along the bottom bar, with a
  live zoom percentage.
- **Terminal nodes** — real PTY (`portable-pty`) through a `vt100` emulator, 256
  color + truecolor + inverse video. Resize the card and the PTY/tmux reflow.
- **Sticky notes** — editable text cards.
- **Direct manipulation** — drag the title bar to move, corner to resize, ✕ to
  close; focused card highlighted, status dot per node type.
- **Persistence** — full layout (canvases, node positions/sizes, notes, pan/zoom)
  saved to `~/.rs-term/layout.json`.

## Keyboard & mouse

| Input                     | Action                          |
|---------------------------|---------------------------------|
| `⌘T` / `⌘N`               | New terminal / note             |
| `⌘W`                      | Close focused node              |
| `⌘S`                      | Save layout                     |
| Scroll                    | Zoom to cursor                  |
| **⌥ + two-finger scroll** | Pan the canvas                  |
| Drag empty space          | Pan the canvas                  |
| Right-click empty space   | "New terminal / note here" menu |

Inside a focused terminal, all normal keys — `Ctrl` combos, arrows, Enter, Tab,
Backspace, and paste — go to the shell.

## Build & run

```bash
cargo run --release          # launch the app
cargo run -- --selftest      # headless PTY+vt100 pipeline check (no window)
```

Requires a Rust toolchain (`rustup`, stable) and, for persistence, `tmux`.

## Architecture

```
src/main.rs      entry point + headless self-test
src/app.rs       eframe App: tabbed canvases, pan/zoom, interaction, rendering
src/node.rs      Node model + serde persistence (SavedApp / SavedCanvas)
src/terminal.rs  PtyTerminal: tmux session in a PTY, reader thread, vt100 state,
                 tmux poller that produces the live title summary
```

Each canvas owns its `Vec<Node>` and its own pan/zoom. World↔screen goes through
one affine transform (`screen = world * zoom + offset`). Every terminal owns a
reader thread (bytes → shared `vt100::Parser`) and a poller thread (tmux →
title summary), both waking the UI via `egui::Context::request_repaint`.

## Scope

Implements nodeterm's spatial terminal canvas + tmux persistence + tabs. The
upstream product additionally has AI-agent orchestration, kanban views,
SSH/remote projects, an iOS companion, and voice dictation — out of scope here.

## License

MIT (this reimplementation). The original nodeterm is BUSL-1.1.
