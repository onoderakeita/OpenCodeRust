---
type: Design
description: Durable session runner と input inbox の設計経緯・未完了項目。
status: Proposal
tags:
  - session
  - runner
  - inbox
  - design
---

# Durable Session Runner & Inbox 設計

## 文書の位置付け

この文書は runner 統合の設計案であり、現行動作の説明ではない。
現行の durable input queue の採用判断と実装制約は
[ADR-0004](docs/adr/adr-0004-durable-session-input-queue.md) を参照する。

## 実装済みの範囲

- SQLite `session_input` に prompt と実行 metadata を保存する。
- Queue item を `pending` から `processing` に条件付きで claim する。
- Session 単位の順序、起動時 recovery、成功・失敗 status を管理する。
- Server route が通常の prompt engine を呼び出して queue item を処理する。

## 未完了の設計課題

- `opencode-session::SessionRunner` は通常の server prompt route に統合
  されていない。queue worker の実行主体としても使われていない。
- Runner と通常 prompt が同じ message、permission、event、error 経路を
  共有する構造への整理が必要。
- Session snapshot と queue admission/message persistence は、まだ同じ
  SQLite transaction ではない。
- 複数 server process にまたがる session state の調停は未対応。

## 次の設計条件

1. Queue と通常 prompt が共通の prompt executor を使う。
2. Message identity を再起動後も安定させ、重複を避ける。
3. Claim、retry、terminal failure の遷移を repository API に集約する。
4. Crash point ごとの回復と複数 worker の振る舞いをテストする。

## 参照

- [Project architecture](docs/architecture.md)
- [ADR-0004: Durable session input queue](docs/adr/adr-0004-durable-session-input-queue.md)
- `crates/opencode-session/src/runner.rs`
- `crates/opencode-server/src/routes.rs`
