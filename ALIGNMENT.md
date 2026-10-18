---
type: Plan
title: "official 移植・整合計画"
description: "公式 OpenCode と OpenCodeRust の機能整合の現況と残作業。"
status: Active
tags:
  - alignment
  - roadmap
  - compatibility
verified:
  by: opencode
  at: "2026-10-18T11:19:00+09:00"
---

## 目標

`OpenCodeRust` (Rust) の性能と安定性を維持しつつ、公式 OpenCode
(TypeScript) の仕様、プロトコル、機能セットに整合させる。

## 戦略

1. Rust ベースの核心エンジン (core crate) を定義し、公式仕様に基づく
   モジュール化を行う。
2. 機能ごとに公式 OpenCode のアーキテクチャ (packages/*) を Rust で
   実装・移植する。
3. 公式仕様を Rust コードの実装基準とする。

## 移行フェーズの現況

下表は 2026-10-18 時点で workspace とテストにより確認した範囲。

| フェーズ | 状態 |
| --- | --- |
| Phase 1: 仕様理解と Rust マッピング策定 | 完了 |
| Phase 2: コア機能 (session/storage) の同期 | 主要実装済み |
| Phase 3: エージェント機能の実装と検証 | 進行中 |

根拠と残作業:

- Phase 1: `MAPPING.md` と `docs/architecture.md` が現行構成を記述済み。
- Phase 2: `opencode-session` / `opencode-storage` に主要実装がある
  (ADR-0004)。残作業は prompt 経路と queue worker が同一 executor を
  共有する整理 ([DESIGN_RUNNER.md](DESIGN_RUNNER.md) の設計条件 1〜4)。
- Phase 3: `opencode-agent` / `opencode-tool` は実装・テスト済み
  (workspace テスト 388 件 green、2026-10-18 実行)。残作業は上流
  checkout との差分照合と未移植機能の特定。

## 参照元

- 公式リポジトリ: `github.com/anomalyco/opencode` の local checkout
  (`~/projects/opencode-official`、参照時 commit `941e71d`、2026-08-10)。
- Rust 開発指針: [AGENTS.md](AGENTS.md)、
  [docs/architecture.md](docs/architecture.md)。

## 記録ルール

公式との機能差を更新するときは、比較対象の upstream commit を記録し、
実装・テストで確認した事実と提案を分けて書く
([MAPPING.md](MAPPING.md) と同一方針)。
