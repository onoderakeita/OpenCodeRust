---
type: Index
description: Architecture、crate reference、ADR をまとめる文書索引。
tags: [documentation, index, adr]
---

# OpenCodeRust documentation index

This documentation set describes the current **OpenCodeRust** codebase
(version `0.1.0`).

## Quick links

- Project overview: [README](../README.md)
- Project architecture: [Architecture](architecture.md)
- Compatibility mapping: [MAPPING](../MAPPING.md)
- Runner design resolution: [DESIGN_RUNNER](../DESIGN_RUNNER.md)
- User guide: [User guide](../USER_GUIDE.md)
- CLI: [CLI](opencode-cli.md)
- TUI: [TUI](opencode-tui.md)
- Server: [Server](opencode-server.md)

## Architecture Decision Records (ADR)

- [ADR-0000: Rust再実装プロジェクト憲章](adr/20260115-rust-reimplementation-charter.md)
- [ADR-0001: HTTP認証必須化とCORS強化](adr/adr-0001-auth-cors.md)
- [ADR-0002: ツール実行権限フロー](adr/adr-0002-permission-flow.md)
- [ADR-0003: プロンプト経路へのツール・システムプロンプト注入](adr/adr-0003-prompt-injection.md)
- [ADR-0004: Durable session input queue](adr/adr-0004-durable-session-input-queue.md)
- [ADR-0005: 質問フロー（question tool / question.prompt）の配線](adr/adr-0005-question-flow.md)

## Module documentation

- [Agent](opencode-agent.md) – Registration, execution, and messages
- [CLI](opencode-cli.md) – `opencoderust` command and subcommands
- [Command](opencode-command.md) – Slash commands and rendering
- [Config](opencode-config.md) – Configuration loading and merging
- [Core](opencode-core.md) – Event bus and ID infrastructure
- [Grep](opencode-grep.md) – Code and text search
- [LSP](opencode-lsp.md) – LSP client and registry
- [Local LLM](opencode-local-llm.md) – Ollama-backed stdio MCP server
- [MCP](opencode-mcp.md) – Client, OAuth, and transport layer
- [Permission](opencode-permission.md) – Rules and decision engine
- [PTY](opencode-pty.md) – Pseudo-terminal process sessions
- [Plugin](opencode-plugin.md) – Hooks and TypeScript subprocess bridge
- [Provider](opencode-provider.md) – Multi-provider model adapters
- [Server](opencode-server.md) – HTTP routes and event stream
- [Session](opencode-session.md) – Lifecycle and message flow
- [Storage](opencode-storage.md) – SQLite storage and repositories
- [Tool](opencode-tool.md) – Built-in tools and registry
- [TUI](opencode-tui.md) – Terminal UI architecture and interaction
- [Types](opencode-types.md) – Shared data types
- [Util](opencode-util.md) – Filesystem, logging, and utilities
- [Watcher](opencode-watcher.md) – Filesystem watcher
- [Security audit](security-audit.md) – cargo-audit runbook and triage
- [Termux build](TERMUX_BUILD.md) – Android build notes

## Code and documentation conventions

- The executable name is `opencoderust`.
- Source code and `--help` output are the behaviour sources of truth.
- Project structure is documented in [Architecture](architecture.md).
- Compatibility plans are in [ALIGNMENT](../ALIGNMENT.md) and [MAPPING](../MAPPING.md).
