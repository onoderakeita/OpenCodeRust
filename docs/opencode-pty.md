---
type: Reference
description: PTY の生成、子 process、terminal input/output を説明する。
tags: [pty, terminal, process, crate]
---

# opencode-pty

`opencode-pty` wraps native pseudo-terminal allocation and exposes sessions for
command execution and output streaming.

## Responsibilities

- Allocate a PTY pair and spawn a command.
- Expose the master writer for interactive input.
- Stream terminal output through a Tokio channel.

## Core API

- `PtyManager::new` creates the native PTY manager.
- `PtyManager::spawn` starts a command with the default terminal dimensions.
- `PtySession::stream_output` reads output on a blocking worker and forwards
  UTF-8-lossy chunks through a bounded channel.

Implementation: `crates/opencode-pty/src/lib.rs`.
