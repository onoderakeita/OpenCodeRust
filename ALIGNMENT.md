---
type: Plan
description: 公式 OpenCode と OpenCodeRust の機能整合計画。
tags: [alignment, roadmap, compatibility]
---

# OpenCodeRust への official 移植・整合計画

## 目標
`OpenCodeRust` (Rustベース) の高いパフォーマンスと安定性を維持しつつ、公式 `opencode-official` (TypeScriptベース) の仕様、プロトコル、機能セットに整合させる。

## 戦略
1. Rustベースの核心エンジン (Core Crate) を定義し、公式仕様に基づくモジュール化を行う。
2. 機能ごとに `opencode-official` のアーキテクチャ (packages/*) を Rust で実装・移植する。
3. 公式仕様 (specs/) をRustコードの実装基準とする。

## 移行フェーズ
- [ ] Phase 1: 仕様の理解と Rust マッピングの策定
- [ ] Phase 2: コア機能 (Repository, Session) の同期
- [ ] Phase 3: エージェント機能の Rust 実装と検証

## 参照元
- 公式リポジトリ: `~/projects/opencode-official`
- Rust 開発指針: `GEMINI.md` (Rust排他的使用)
