---
type: Reference
description: `opencoderust` CLI の入口、コマンド、実行モードを説明する。
tags: [cli, commands, crate]
---

# opencode-cli

`opencode-cli` provides the workspace’s unified executable entrypoint
(binary name: `opencoderust`).

## Command scope

- Start TUI
- Start or attach to server
- Run single tasks (`run`)
- Invoke session, model, MCP, debug, and other subcommands

## Top-level subcommands

As of `opencoderust --help` (2026-10-16):

- `tui`
- `attach`
- `run`
- `serve`
- `web`
- `acp`
- `models`
- `session`
- `stats`
- `db`
- `config`
- `auth`
- `agent`
- `debug`
- `mcp`
- `export`
- `import`
- `github`
- `pr`
- `upgrade`
- `uninstall`
- `generate`
- `version`

## Common options

### `opencoderust tui`

- `-m, --model <MODEL>`
- `-c, --continue`
- `-s, --session <SESSION>`
- `--fork`
- `--agent <AGENT>` (default: `build`)
- `--port <PORT>`, `--hostname <HOSTNAME>`

### `opencoderust run`

- `MESSAGE...`
- `--command <COMMAND>`
- `-f, --file <FILE>`
- `--format <default|json>`
- `--thinking`
- `--agent <AGENT>` / `--model <MODEL>`

## Source entrypoint

- `crates/opencode-cli/src/main.rs`

## Development notes

- After changing subcommand behaviour, update `--help` and then the docs
- Prefer consistent naming between CLI args and server/config fields

## Validation

```bash
cargo check -p opencode-cli
./target/debug/opencoderust --help
```
