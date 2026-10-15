---
type: Playbook
description: OpenCodeRust の利用者向けセットアップと操作ガイド。
tags: [user-guide, setup, usage]
---

# USER GUIDE - RustingOpenCode (ROCode)

This guide is for day-to-day users. It covers startup, common commands, configuration, and troubleshooting.  
The project is named `RustingOpenCode` (short: `ROCode`). The CLI command is `opencoderust`.

**Use this project's `opencoderust` binary.** From the
**RustingOpenCode repo root** use one of:

- **`./target/debug/opencoderust`** — after `cargo build -p opencode-cli`
- **`cargo run -p opencode-cli --`** — always uses this repo's version

Example: `./target/debug/opencoderust tui`
or `cargo run -p opencode-cli -- tui`.  
In the commands below, `opencoderust` means this local binary (run from repo root).

## 1. Quick start

From the RustingOpenCode repo root:

```bash
cargo run -p opencode-cli -- --help
```

Start TUI by default:

```bash
cargo run -p opencode-cli --
```

Same as:

```bash
cargo run -p opencode-cli -- tui
```

Single non-interactive run:

```bash
cargo run -p opencode-cli -- run "Summarise the current risks in this repo"
```

Start HTTP server:

```bash
cargo run -p opencode-cli -- serve --port 3000 --hostname 127.0.0.1
```

## 2. Common commands

Run these from the **RustingOpenCode repo root**; use
`./target/debug/opencoderust` or `cargo run -p opencode-cli --`.

### 2.1 Session management

```bash
./target/debug/opencoderust session list
./target/debug/opencoderust session list --format json
./target/debug/opencoderust session show <SESSION_ID>
./target/debug/opencoderust session delete <SESSION_ID>
```

### 2.2 Models and config

```bash
./target/debug/opencoderust models
./target/debug/opencoderust models --refresh --verbose
./target/debug/opencoderust config
```

### 2.3 Auth management

```bash
./target/debug/opencoderust auth list
./target/debug/opencoderust auth login --help
./target/debug/opencoderust auth logout --help
```

Note: For `auth login` / `auth logout` options, use the `--help` output.

### 2.4 MCP management

```bash
./target/debug/opencoderust mcp list
./target/debug/opencoderust mcp connect <NAME>
./target/debug/opencoderust mcp disconnect <NAME>
./target/debug/opencoderust mcp add --help
./target/debug/opencoderust mcp auth --help
```

If the local server is not at the default address:

```bash
./target/debug/opencoderust mcp --server http://127.0.0.1:3000 list
```

### 2.5 Debug commands

```bash
./target/debug/opencoderust debug paths
./target/debug/opencoderust debug config
./target/debug/opencoderust debug skill
./target/debug/opencoderust debug agent
```

## 3. TUI and Run common options

Show full options:

```bash
./target/debug/opencoderust tui --help
./target/debug/opencoderust run --help
```

Frequently used (both TUI and run):

- `-m, --model <MODEL>` – Set model
- `-c, --continue` – Continue latest session
- `-s, --session <SESSION>` – Continue specific session
- `--fork` – Fork session
- `--agent <AGENT>` – Set agent (default: `build`)
- `--port <PORT>` / `--hostname <HOSTNAME>` – Server address

Additional common options for `run`:

- `--format default|json`
- `-f, --file <FILE>`
- `--thinking`

## 4. Config file locations

The program merges config from multiple locations by priority (searching upward):

- `opencode.jsonc`
- `opencode.json`
- `.opencode/opencode.jsonc`
- `.opencode/opencode.json`

Global config default:

- `~/.config/opencode/opencode.jsonc` (or `.json`)

Recommendation: Start with a minimal project-level config, then add provider/mcp/agent/lsp as needed.

## 5. Using Claude (Anthropic) and testing a coding session

Claude is supported via the **anthropic** provider. You can run interactive coding in the TUI or a one-off task with `run`.

### 5.1 Set your API key

**Option A – Environment variable (easiest):**

```bash
export ANTHROPIC_API_KEY="sk-ant-your-key-here"
```

Get a key at [Anthropic Console](https://console.anthropic.com/).

**Option B – Config file:**

Create or edit `opencode.jsonc` in your project (or `~/.config/opencode/opencode.jsonc`):

```jsonc
{
  "model": "anthropic/claude-sonnet-4-20250514",
  "provider": {
    "anthropic": {
      "api_key": "sk-ant-your-key-here"
    }
  }
}
```

Do not commit real API keys; use env vars or a local config that’s in `.gitignore`.

### 5.2 Check that the provider is available

```bash
./target/debug/opencoderust auth list
./target/debug/opencoderust models --refresh
```

You should see `anthropic` and Claude models in the list.

### 5.3 Run a coding session

**Interactive TUI (recommended for coding):**

```bash
./target/debug/opencoderust tui -m anthropic/claude-sonnet-4-20250514
```

Or with default model from config:

```bash
./target/debug/opencoderust tui
```

In the TUI you can type prompts, use slash commands, and let the agent edit files and run tools.

**Single task (no TUI):**

```bash
./target/debug/opencoderust run "Add a unit test for the login function in src/auth.rs"
```

With a specific model:

```bash
./target/debug/opencoderust run -m anthropic/claude-sonnet-4-20250514 "Review this repo for security issues"
```

### 5.4 Useful model IDs (Anthropic)

- `anthropic/claude-sonnet-4-20250514` – Claude Sonnet 4
- `anthropic/claude-3-5-sonnet-20241022` – Claude 3.5 Sonnet
- `anthropic/claude-3-opus-20240229` – Claude 3 Opus

Use `./target/debug/opencoderust models` to see the full list for your setup.

## 6. Recommended workflows

### 6.1 Local interactive use

1. `cargo run -p opencode-cli --` or `./target/debug/opencoderust tui`
2. Run tasks in the TUI
3. Use `./target/debug/opencoderust session list/show` to review history

### 6.2 Scripts or integration

1. `./target/debug/opencoderust serve --port 3000`
2. Use `./target/debug/opencoderust run ... --format json` or
   the server API for integration
3. Use `./target/debug/opencoderust stats` to track token/cost

## 7. Troubleshooting

### 7.1 Port in use

- Use another port: `./target/debug/opencoderust serve --port 3001`

### 7.2 Model not available

1. `./target/debug/opencoderust auth list`
2. `./target/debug/opencoderust models --refresh`
3. `./target/debug/opencoderust config` to check that provider config is applied

### 7.3 Config issues

1. `./target/debug/opencoderust debug paths` – see config search paths
2. `./target/debug/opencoderust debug config` – see final merged config

### 7.4 MCP connection problems

1. `./target/debug/opencoderust mcp list`
2. `./target/debug/opencoderust mcp debug <NAME>`
3. `./target/debug/opencoderust mcp connect <NAME>`

## 8. Documentation index

- Project overview: `README.md`
- Docs index: `docs/README.md`
- CLI: `docs/opencode-cli.md`
- TUI: `docs/opencode-tui.md`
- Config: `docs/opencode-config.md`
