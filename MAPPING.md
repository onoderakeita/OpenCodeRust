---
type: Reference
title: "公式 OpenCode と Rust 実装のマッピング"
description: "upstream packages と OpenCodeRust crate の対応、欠落領域、命名差を記録する。"
status: Current
tags:
  - mapping
  - compatibility
  - roadmap
verified:
  by: opencode
  at: "2026-10-18T14:27:00+09:00"
---

## 文書の位置付け

upstream checkout (`github.com/anomalyco/opencode` @ `941e71d`、2026-08-10)
に対する 2026-10-18 時点の対応調査である。掲載した対応と欠落は checkout の
path で確認した事実に基づく。旧版の初期計画 (specs/ 前提) は git 履歴を
参照。フェーズ計画は [ALIGNMENT.md](ALIGNMENT.md)。

## upstream の構造前提

upstream は v1 monolith (`packages/opencode/src/*`) と v2 engine
(`packages/core/src/*`) の二層で、`packages/` は 32 ディレクトリ。
Rust 側の crate 分割は v1 の subsystem 構成と概ね 1:1 で対応する。

## 対応 (高確度)

| upstream | Rust 側 |
| --- | --- |
| `opencode/src/{session,tool,agent,...}` | 同名 `opencode-*` crate |
| `packages/cli` | `opencode-cli` |
| `packages/server` | `opencode-server` |
| `packages/tui` | `opencode-tui` (別実装: ratatui) |
| `packages/llm` | `opencode-provider` |
| `packages/schema` | `opencode-types` |
| `packages/plugin` | `opencode-plugin` |
| `packages/core` (v2 engine) | `opencode-core` ほか (責務別) |

v1 `opencode/src` の subsystem (`agent`、`session`、`tool`、`permission`、
`pty`、`lsp`、`mcp`、`storage`、`config`、`command`) は同名 crate に存在する。

## 欠落・未分化の領域

未実装または対応層が未分化の主な領域 (重要性順):

1. `codemode` — parser基盤、対応構文検査、値境界、実行制限、同期評価器、
   stdlibの一部、同期tool境界 (`opencode-codemode`)。非同期実行と
   実時間timeoutは未実装。
2. `protocol` / `client` / `httpapi-codegen` — 権威 HTTP 契約と生成
   client 層が未分化 (Rust は `opencode-server` の routes が唯一の面)。
3. `sdk` / `sdk-next` — JS SDK がない。
4. `function` (cloud sync) と v1 `opencode/src/sync` — 同期基盤がない。
5. `http-recorder` — HTTP/WS リプレイによるテスト基盤がない。
6. `opencode/src/acp` — Rust は外部 ACP ブリッジ起動のみ (HTTP fallback)。
7. v2 `core` の policy / location / integration / credential /
   observability 層 — 対応 crate がない。

## 命名差

- upstream `core` = v2 engine 全体、Rust `opencode-core` = bus / fs /
  git / id / repository。同名だが責務が異なる。
- 共有型は upstream `schema` ↔ Rust `opencode-types`。
- Rust 独自の crate 名: `opencode-grep`、`opencode-watcher`、
  `opencode-pty`、`opencode-command` (upstream は `core/src` の module)。
- `opencode-local-llm` は upstream に対応物がない (Ollama 用 stdio MCP)。
- バイナリ名は `opencoderust` (upstream は `opencode`)。

## 更新手順

upstream を更新したら、本表の commit 記録を更新し、対応と欠落の状態を
checkout の path で再確認する。
