---
name: rust-tools
description: Run Rust formatter (rustfmt), linter (clippy), LSP (rust-analyzer), and build (cargo) automatically. Use when editing Rust files, before committing, or when the user wants to verify code quality.
---

# Rust Tools

Automated quality checks for Rust projects using standard toolchain.

## Quick Commands

Run from workspace root:

```bash
# Format (rustfmt)
cargo fmt --all

# Lint (clippy)
cargo clippy --workspace --all-targets -- -D warnings

# Build
cargo build --workspace

# Test
cargo test --workspace

# Full check (format + lint + build + test)
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo build --workspace && cargo test --workspace
```

## LSP (rust-analyzer)

The project uses `rust-analyzer` for LSP support. Configure in your editor:

- VS Code: Install `rust-analyzer` extension
- Neovim: Use `nvim-lspconfig` with `rust_analyzer`
- Helix: Built-in support

No additional config needed - uses `Cargo.toml` and `rustfmt.toml`.

## Configuration Files

| File | Purpose |
|------|---------|
| `rustfmt.toml` | Formatter rules |
| `Cargo.toml` | Workspace lints (`warnings = "deny"`, `clippy::all = "deny"`) |
| `.github/workflows/ci.yml` | CI pipeline (if exists) |

## Automated Workflow

### On File Save (via LSP)
1. `rust-analyzer` provides diagnostics
2. `rustfmt` formats on save (if editor configured)

### Before Commit
```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### CI Pipeline
Run on every PR:
- Format check
- Clippy with deny warnings
- Build all targets
- Test suite

## Tool Versions (pinned in CI)

```toml
# rust-toolchain.toml (optional - uncomment to pin)
# [toolchain]
# channel = "stable"
# components = ["rustfmt", "clippy"]
```

## Scripts

- `scripts/check-all.sh` — Full verification pipeline
- `scripts/fix-all.sh` — Auto-fix format + clippy suggestions

## Usage

**Format all code:**
```bash
./scripts/fix-all.sh
```

**Verify everything passes:**
```bash
./scripts/check-all.sh
```

## Reference

- [rustfmt config](https://rust-lang.github.io/rustfmt/)
- [clippy lints](https://rust-lang.github.io/rust-clippy/)
- [rust-analyzer](https://rust-analyzer.github.io/)