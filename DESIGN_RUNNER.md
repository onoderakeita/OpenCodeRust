---
type: Design
title: "Durable Session Runner & Inbox 設計 (解決済み)"
description: "runner/inbox 設計の結末と、現行 executor・queue 状態機械の所在を記録する。"
status: Superseded
tags:
  - session
  - runner
  - inbox
  - design
verified:
  by: opencode
  at: "2026-10-18T13:36:00+09:00"
---

## 文書の位置付け

この文書は runner 統合の設計案だった。現在は解決済みの記録であり、
queue の現行仕様は
[ADR-0004](docs/adr/adr-0004-durable-session-input-queue.md) を正とする。
現行動作はこの文書では説明しない。

## 解決の記録 (2026-10-18)

| 設計条件 | 結末 |
| --- | --- |
| Queue と通常 prompt の共通 executor | server の `spawn_prompt_execution` |
| Message identity の安定 | queue ID = user message ID。回復時は既存を再利用 |
| 遷移の repository 集約 | `InputRepository` (claim / promote / fail / reset) |
| 回復と claim のテスト | repository の queue lifecycle テスト + 起動時 resume |

`opencode-session::SessionRunner` は未使用の重複スタブだったため削除した
(2026-10-18)。queue worker の実行主体は server であり、session crate は
session/message model と prompt 実行路を提供する。

## 残る設計制約 (ADR-0004 より)

- Queue admission と session snapshot の永続化は同一 SQLite transaction
  ではない。安定 ID と再利用で回復する。
- 同一 database を使う server は 1 process に限定する。
- provider 失敗は terminal (`failed`)。再開は手動 re-enqueue。

## 参照

- [ADR-0004](docs/adr/adr-0004-durable-session-input-queue.md)
- [Project architecture](docs/architecture.md)
- `crates/opencode-server/src/routes.rs` (executor / resume_queued_prompts)
- `crates/opencode-storage/src/repository.rs` (状態機械)
