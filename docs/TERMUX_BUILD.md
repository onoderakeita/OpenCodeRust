---
type: Playbook
description: Termux/Android 環境での build 制約と回避策を記録する。
tags: [termux, android, build]
verified: "2026-10-16T00:30:00+09:00"
---

# Rust Build Notes for Termux/Android

This document captures the specific workarounds required to build
OpenCodeRust in Termux.

## Vendored Patches (workspace level)

現在なし。以前の `termios 0.2.2` Android workaround は、`portable-pty 0.9`
が `serial2`/`nix` ベースへ移行して `termios` 依存が消えたため削除した。

## Removed Dependencies

### html2md (was `opencode-tool`)

- **Problem:** `html2md 0.2.17` fails to compile on Android (its `jni`
  dependency expects a full JVM environment for `get_string`/`new_string`).
- **Solution:** Removed the dependency. `webfetch` HTML-to-markdown conversion
  now uses the in-repo `strip_html` fallback, which already existed for the
  `text` format.

## Lifecycle Standards

- After changing a dependency version, run `cargo clean -p <crate>` if stale
  build artifacts cause confusion.
- Verify binary location using find ./target/release -name <binary_name>.
- Use --release for production builds to ensure binary stripping and optimization.
