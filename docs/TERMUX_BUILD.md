---
type: Playbook
description: Termux/Android 環境での build 制約と回避策を記録する。
tags: [termux, android, build]
---

# Rust Build Notes for Termux/Android

This document captures the specific workarounds required to build OpenCodeRust in Termux.

## Build Patching (jni v0.19.0)

- **Problem:** build.rs fails due to incorrect target_os handling and missing EXPECTED_JVM_FILENAME constants.
- **Solution:** Edit the build script to explicitly set the JVM filename for Linux/Android targets.

## Platform Imports (termios v0.2.2)

- **Problem:** Unresolved imports in src/os/mod.rs for Android targets.
- **Solution:** Edit src/os/mod.rs to explicitly export the linux module for target_os == "android".

## Lifecycle Standards

- Always run cargo clean after patching dependencies.
- Verify binary location using find ./target/release -name <binary_name>.
- Use --release for production builds to ensure binary stripping and optimization.
