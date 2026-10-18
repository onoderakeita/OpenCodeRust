---
type: Reference
description: terminal UI の画面、操作、theme 構成を説明する。
tags: [tui, terminal, crate]
---

# opencode-tui

`opencode-tui` provides the terminal UI: home screen, session view, input, sidebar, dialogs, shortcuts, and theme system.

## Branding and display

- `APP_NAME`: `OpenCodeRust`
- `APP_SHORT_NAME`: `opencoderust`
- `APP_VERSION_DATE`: `2026.02.23`
- `APP_TAGLINE`: `A Rust implementation of OpenCode`

Defined in: `crates/opencode-tui/src/branding.rs`

## Responsibilities

- Render session messages and tool results
- Manage input, completion, command palette, and dialogs
- Connect to server API and local event loop
- Theme, layout, and interaction state

## Key modules

- `app/` – Main event loop and state sync
- `components/` – home / session / prompt / sidebar / dialog
- `context/` – App state, key bindings, cache
- `api.rs` – Client for local server
- `file_index.rs` – `@path` completion index (nucleo matcher)
- `components/markdown/` – Code block rendering and syntect highlighting

## Current enhancements

- Overlay sidebar with explicit toggle (including `☰` button)
- Braille/KnightRider switchable spinner
- Refined message block layout and status line
- Syntect code highlighting and path-aware completion
- Permission and question prompts fed from `/event` SSE
  (`permission.requested` / `question.asked`)

## Development notes

- UI changes should preserve scroll stability and low CPU usage
- Mouse handling should be tested for hover + scroll
- Text rendering must use character-boundary-safe handling (avoid UTF-8 slice panics)

## Validation

```bash
cargo check -p opencode-tui
```
