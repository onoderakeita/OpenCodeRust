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

Patched third-party crates live under `vendor/` and are wired in via
`[patch.crates-io]` in the workspace `Cargo.toml`. This keeps the fixes
reproducible without editing `~/.cargo/registry` by hand.

### termios v0.2.2 (`vendor/termios-0.2.2`)

- **Problem:** The crate ships `os/{linux,macos,freebsd,openbsd}.rs` but no
  `os/android.rs`. On `target_os = "android"` the module resolution for
  `os::target` fails and the crate does not compile. It is pulled in through
  `portable-pty -> serial -> serial-unix -> termios`, and `serial` is a
  non-optional dependency, so it cannot be avoided via feature flags.
- **Solution:** Vendor the crate and add `os/android.rs` mirroring the Linux
  definitions. Android's bionic libc follows the Linux termios ABI
  (`NCCS = 32`, same struct layout and constant values), so the Linux
  definitions are the correct target behavior.

## Removed Dependencies

### html2md (was `opencode-tool`)

- **Problem:** `html2md 0.2.17` fails to compile on Android (its `jni`
  dependency expects a full JVM environment for `get_string`/`new_string`).
- **Solution:** Removed the dependency. `webfetch` HTML-to-markdown conversion
  now uses the in-repo `strip_html` fallback, which already existed for the
  `text` format.

## Lifecycle Standards

- After changing `vendor/`, run `cargo clean -p <crate>` before rebuilding.
- Verify binary location using find ./target/release -name <binary_name>.
- Use --release for production builds to ensure binary stripping and optimization.
