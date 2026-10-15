---
type: Reference
description: HTTP/SSE/WebSocket API と server orchestration を説明する。
tags: [server, api, session, crate]
---

# opencode-server

`opencode-server` provides HTTP/SSE/WebSocket APIs and bridges CLI, TUI, and external systems.

## Responsibilities

- Expose unified API routes
- Manage sessions, config, providers, MCP, permissions, files, and search
- Provide event stream and TUI control endpoints
- Handle OAuth callbacks, PTY, and workspace operations
- Admit asynchronous session prompts through the durable SQLite input queue

## Route groups (selection)

As in `crates/opencode-server/src/routes.rs`:

- Base: `/health`, `/event`, `/path`, `/vcs`
- Session: `/session/*`
- Provider: `/provider/*`
- Config: `/config/*`
- MCP: `/mcp/*`
- File: `/file/*`
- Search: `/find/*`
- Permission: `/permission/*`
- Question: `/question/*`（保留中の質問一覧、回答、拒否）
- Project: `/project/*`
- PTY: `/pty/*`
- TUI control: `/tui/*`
- Experimental: `/experimental/*`
- Plugin auth: `/plugin/*`

## Module structure

- `server.rs` – Server startup and lifecycle
- `routes.rs` – Route definitions and handlers
- `oauth.rs` / `mcp_oauth.rs` – OAuth flow
- `pty.rs` – Terminal session bridge
- `worktree.rs` – Workspace operations

## Development notes

- Define input/output models before adding routes and handlers
- Avoid blocking (I/O, DB, network) on high-concurrency paths
- Keep CLI/TUI call sites in sync when changing APIs
- Queue execution is serialized per session and recovered from pending input
  during server startup; use only one server process per SQLite database

See [ADR-0004](adr/adr-0004-durable-session-input-queue.md) for queue states,
ordering, recovery, and operational constraints.

## Validation

```bash
cargo check -p opencode-server
```
