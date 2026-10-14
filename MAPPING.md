---
type: Plan
description: 公式 OpenCode と OpenCodeRust の機能整合に関する初期計画。
status: Planning
tags:
  - mapping
  - compatibility
  - roadmap
---

# 公式仕様と Rust 実装のマッピング

## 文書の位置付け

この文書は初期の移行計画であり、現行実装との対応表ではない。
記載された配置案や未完了項目を、現在の実装状態として扱わないこと。
現行 workspace の責務は [Project architecture](docs/architecture.md)、
crate 別の記述は [docs index](docs/README.md) を参照する。

## 初期計画の対象

- Session create/prompt/interrupt と storage の責務整理。
- Context、system prompt、session event の構造化。
- Durable session input inbox の追加。
- TypeScript/Effect の非同期設計を Rust/Tokio に適合させること。

Durable input queue の現行決定は
[ADR-0004](docs/adr/adr-0004-durable-session-input-queue.md) に分離している。

## 参照の制約

この repository には、旧版で参照していた `specs/v2/session.md` は存在
しない。公式との機能差を更新するときは、比較対象の公式 checkout と
version/commit を別途記録し、実装・テストで確認した事実と提案を分ける。
