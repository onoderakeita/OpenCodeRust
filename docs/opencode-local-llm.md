---
type: Reference
description: Ollama 向け local LLM MCP server binary の実装を説明する。
tags: [local-llm, ollama, mcp, crate]
---

# opencode-local-llm

`opencode-local-llm` is a standalone stdio MCP server binary for querying
local models through Ollama. It is not a library crate in the workspace.

## Responsibilities

- Read newline-delimited JSON-RPC messages from standard input.
- Advertise MCP tool capability and local-model query tools.
- Execute Ollama requests and return JSON-RPC responses on standard output.

## Runtime boundary

The binary uses the local Ollama command-line interface. Configure and run it
as an MCP stdio server; keep protocol output on standard output and diagnostic
messages on standard error.

Implementation: `crates/opencode-local-llm/src/main.rs`.
