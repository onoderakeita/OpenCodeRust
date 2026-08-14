# 公式仕様と Rust 実装のマッピング (Phase 1)

公式の `specs/v2/session.md` に基づき、Rust 実装へのマッピング計画を定義する。

## Session Core
- `sessions.create` / `sessions.prompt` / `sessions.interrupt`: `opencode-session` (Rust) の `Session` 型および `Service` インターフェースへマッピング。
- `SessionExecution.resume`: `opencode-agent` (Rust) の `executor.rs` へロジックを移行。
- `SessionStore`: `opencode-storage` (Rust) へマッピング。

## Context Management
- `Context Epoch`: `opencode-session/src/prompt.rs` または `instruction.rs` に「エポック」の概念を Rust の構造体として定義。
- `System Context Registry`: `opencode-config` または `opencode-core` の機能として統合を検討。

## Data & Event Persistence
- `session_input` (durable admission inbox): `opencode-storage` の SQL スキーマとして定義。
- `Session History` / `Events`: `opencode-core` の `bus.rs` または専用のイベントストアとして Rust で実装。

## 課題
- TypeScript/Effect ベースのリアクティブな設計を、Rust の `async/await` および `tokio` (または適切な非同期ランタイム) で同等の堅牢性を持って再構築する必要がある。
- `sst.config.ts` に依存するインフラ部分を、Rust 側の環境設定とどう整合させるか。
