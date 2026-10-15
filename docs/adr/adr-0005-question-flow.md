---
type: ADR
title: 質問フロー（question tool / question.prompt）の配線
description: QuestionBroker、質問コールバック、TUI への question/permission イベント配送を定義する。
status: Accepted
tags:
  - session
  - server
  - tui
  - question
  - permission
generated:
  by: OpenCodeRust contributors
  at: "2026-10-16T01:38:47+09:00"
---

## ADR-0005: 質問フロー（question tool / question.prompt）の配線

### Status

Accepted. The implementation is present in the tool, session, server, and TUI
crates. Unit and integration tests plus workspace validation pass.

### Context

`question` ツールと `plan` ツール（plan/build 切替の確認）はユーザー回答を
必要とするが、実行経路が stdin 直読みに固定されていた。サーバ側には
`/question/*` ルートが存在したが、保留リクエストを登録する producer が
なく、`question.asked` イベントも未定義だった。

また `ToolContext::with_ask_question` は未使用で、`plan` 系ツールの
`ctx.question()` は常に "Question callback not configured" で失敗していた。
TUI 側も SSE イベントの `question.asked` / `permission.requested` を
解釈せず、`PermissionPrompt::add_request` の呼び出し元が存在しなかった。

### Decision

#### 1. QuestionBroker（サーバ側）

ADR-0002 の `PermissionBroker` と同一パターンで実装する。

- `oneshot::channel` で保留リクエストを管理
  （`ask(info) -> QuestionResponse` / `reply(id, answers) -> bool` /
  `reject(id) -> bool` / `list() -> Vec<QuestionRequestInfo>`）
- リクエスト情報は `QuestionRequestInfo { id, session_id,
  questions: Vec<opencode_tool::QuestionDef> }`
- イベントバスへ `question.asked` / `question.replied` /
  `question.rejected` をブロードキャスト

#### 2. ServerState 統合とルート接続

- `ServerState.question_broker: Arc<QuestionBroker>` を追加
- `GET /question` → `broker.list()`
- `POST /question/{id}/reply`（`answers: string[][]`）→ `broker.reply()`
- `POST /question/{id}/reject` → `broker.reject()`
- 未登録 ID は `404` を返す

#### 3. プロンプト側コールバック配線（`opencode-session`）

- `QuestionCallback` 型を定義し、`prompt_with_update_hook` と
  `loop_inner` にスレッドする
- `loop_inner` はコールバックが `Some` のときだけ
  `ToolContext::with_ask_question` を設定する
- サーバの `spawn_prompt_execution` が `QuestionBroker` を
  コールバックへ橋渡しし、拒否時は
  `ToolError::QuestionRejected` を返す

#### 4. question ツールの二経路化

- コールバック設定時は `ctx.question()` を使用（サーバ / TUI 経路）
- 未設定時は stdin フォールバック（`opencoderust run` などの直接実行）
- 出力は質問ごとの `answers: string[][]` に統一する

#### 5. TUI へのイベント配送

- SSE `/event` の `question.asked` を解析して `QuestionPrompt` を開く
- 複数質問は順次表示し、回答を蓄積して全問回答後に
  `POST /question/{id}/reply` を送信する
- `Esc` は `POST /question/{id}/reject` を送信して次へ進む
- 併せて `permission.requested` を `PermissionPrompt` に配送し、
  権限プロンプトの受信経路を完成させる

### Consequences

- サーバ経由の質問がユーザー操作で解決され、ツール実行が再開する
- `plan` / `plan_exit` の確認ダイアログが動作する
- CLI 直接実行（`opencoderust run`）は従来どおり stdin で対話する
- TUI が接続していないサーバで質問が発生した場合、回答が得られる
  までツールは待機する（permission と同一の挙動）
- タイムアウトは設けない。ユーザーの回答待ちは正常系であり、
  打ち切ると回答不能なセッションが残るため。停止したい場合は
  セッションを cancel するかサーバを再起動する
- 保留エントリはメモリ上のみで保持する。タスク abort や panic で
  `ask` future が破棄された場合、保留エントリが `list()` に残り得る
  （PermissionBroker と同一の既知の制約。プロセス再起動で消滅する）
- `PermissionBroker` と `QuestionBroker` は現時点で別実装である。
  ペイロードと応答型が異なるため共通化は将来の課題とする
- TUI は回答・拒否 API の失敗をトーストで通知する。空の質問配列を
  受信した場合は即座に拒否し、ツールが待機し続けないようにする

### Validation

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

テスト:

- `opencode-server`: `question_broker_*`（ask/reply/reject/list、イベント）
- `opencode-tool`: `question::tests::*`（コールバック経路、拒否、入力検証）
- `opencode-session`: `question_callback_is_used_by_question_tool_calls`
- `opencode-tui`: `parse_question_asked_*`、`parse_permission_requested_*`、
  `components::question::tests::*`

### References

- [ADR-0002: ツール実行権限フローの配線](adr-0002-permission-flow.md)
