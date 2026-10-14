---
type: ADR
title: OpenCodeRust - Rust再実装プロジェクト憲章
status: Approved
date: 2026-01-15
deciders:
  - OpenCodeRust Team
tags:
  - architecture
  - strategy
  - rust
---

# ADR-0000: OpenCodeをRustで再実装し、上流追随とRustの強みを活かした再構築を行う

## Context

OpenCode（TypeScript/Effect実装）は、AIコーディングエージェントとして成熟しつつあるが、以下の課題がある：

1. **ランタイムオーバーヘッド**: Node.js/Bunランタイム必須、起動遅延、メモリ使用量大
2. **型安全性の限界**: TypeScriptの構造的型付けでは実行時エラーを完全排除困難
3. **並行処理モデル**: Effectは強力だが、ネイティブスレッド・asyncランタイムの恩恵を受けにくい
4. **配布・導入障壁**: npm/bun依存、ネイティブバイナリ配布の複雑さ
5. **システム統合**: プロセス管理、ファイル監視、LSP/MCP通信でのネイティブ性能不足

Rustは以下の強みでこれらを解決可能：

- ゼロコスト抽象化・所有権モデルによるメモリ安全性
- 真の並行処理（data race free）とネイティブasync/await
- 単一バイナリ配布、クロスコンパイル、依存最小化
- C/C++/システムAPIとの直接相互運用
- 型レベルでの不変条件表現（タイプステート、セッション型等）

## Decision

**OpenCodeRust を OpenCode の公式 Rust 再実装として位置づけ、以下を実現する：**

### 1. 互換性維持（上流追随）
- **CLI インターフェース互換**: `opencode` コマンド引数・サブコマンド・フラグを維持
- **API 互換**: HTTP/SSE/WebSocket エンドポイント、JSON スキーマを維持
- **設定互換**: `opencode.jsonc` / `.opencode/opencode.jsonc` 形式を維持
- **セッション形式互換**: SQLite スキーマ・JSON エクスポート形式を維持
- **プラグイン/MCP/LSP プロトコル互換**: 既存エコシステム資産を再利用

### 2. Rust ネイティブ再構築（段階的）
| フェーズ | 対象 | 方針 |
|---------|------|------|
| Phase 1 | Core/Session/Storage/Config | 既存ロジックの忠実移植 + 型安全化 |
| Phase 2 | Tool/Provider/Agent | トレイトベース抽象化、ゼロコスト化 |
| Phase 3 | Server/CLI/TUI | axum/tokio/ratatui ネイティブ実装 |
| Phase 4 | Plugin/MCP/LSP | 非同期トレイト・ゼロコピー通信 |
| Phase 5 | 最適化・独自拡張 | コンパイル時検証、SIMD、独自機能追加 |

### 3. アーキテクチャ原則
- **Workspace 分割**: 21クレートによる関心分離（`opencode-*`）
- **依存方向**: `core` → `types` → `util` → `config/storage/session` → `tool/provider/agent` → `server/cli/tui` → `plugin/mcp/lsp`
- **動的ディスパッチ最小化**: `dyn Trait` は境界のみ、内部は静的ディスパッチ
- **非同期ランタイム**: tokio 固定、`Send + Sync + 'static` 境界厳守
- **エラー型**: `thiserror` でドメイン固有エラー、`anyhow` は境界のみ
- **観測性**: `tracing` 構造化ログ、OpenTelemetry 対応予定

### 4. 上流追随プロセス
- **週次同期**: `opencode` リポジトリ `dev` ブランチの変更を監視
- **差分分類**:
  - `spec-change` → 仕様書・ADR 更新 → 実装反映
  - `bugfix` → 同等修正 + テスト追加
  - `feature` → Rustイディオムで再設計検討 → 実装
  - `refactor` → Rust利点活用可否判断 → 採用/見送り
- **互換性テスト**: 共有フィクスチャ・E2Eシナリオで回帰検証

## Consequences

### Positive
- **単一バイナリ配布**: `cargo build --release` で全プラットフォーム対応
- **高速起動**: <50ms（TUI除く）、ネイティブスレッド並列でツール実行高速化
- **メモリ効率**: 既存比 1/3〜1/5 予想
- **型安全性**: コンパイル時セッション状態検証、プロバイダ切替時の型検査
- **保守性**: クレート境界による責務明確化、リファクタリング容易

### Negative / Risk
- **開発コスト**: TypeScript→Rust 移植に初期投資大（推定 6-12ヶ月）
- **エコシステム差**: 一部 npm パッケージ（tree-sitter bindings 等）でバインディング維持必要
- **貢献障壁**: Rust 学習コストでコントリビューター減の可能性
- **上流追随遅延**: 大規模リファクタ時の追随ラグリスク

### Mitigation
- **段階的移行**: CLI/TUI から着手、サーバ互換で早期価値提供
- **自動化**: `cargo clippy`/`fmt`/`test` CI、依存更新自動化（Renovate）
- **ドキュメント**: アーキテクチャ決定記録（ADR）蓄積、オンボーディングガイド整備
- **互換性ゲート**: PR で API/CLI/セッション形式破壊的変更を自動検知

## Alternatives Considered

| 代替案 | 却下理由 |
|--------|----------|
| **TypeScript 継続 + 最適化** | ランタイム制約根本解決不可、ネイティブ性能不可 |
| **Go 再実装** | ジェネリクス・型システム表現力不足、所有権モデルなし |
| **FFI で Rust コアのみ** | 境界コスト大、二重実装・同期負荷増 |
| **WASM ターゲット** | システムコール・プロセス管理・PTY で制約大 |

## Links

- [OpenCode 原リポジトリ](https://github.com/anomalyco/opencode)
- [Project architecture](../architecture.md)
- [Documentation index](../README.md)
- [ADR-0001: HTTP認証必須化とCORS強化](adr-0001-auth-cors.md)
- [ADR-0002: ツール実行権限フロー](adr-0002-permission-flow.md)
- [ADR-0003: プロンプト経路へのツール・システムプロンプト注入](adr-0003-prompt-injection.md)
- [ADR-0004: Durable session input queue](adr-0004-durable-session-input-queue.md)

---

**Approved by**: OpenCodeRust Team
**Date**: 2026-01-15
**Next Review**: 2026-04-15 (四半期見直し)
