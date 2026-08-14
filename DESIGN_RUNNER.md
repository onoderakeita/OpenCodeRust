# Durable Session Runner & Inbox 設計

公式仕様の `SessionRunner` および `session_input` (Durable Admission Inbox) を Rust で実装するための設計案。

## 1. Inbox (session_input)
Rust側で耐久性を持つ `SessionInbox` を実装する。
- SQLite への永続化を前提とする。
- `Admitted` イベントを記録するテーブル構造:
  ```sql
  CREATE TABLE session_input (
      id UUID PRIMARY KEY,
      session_id UUID NOT NULL,
      prompt TEXT NOT NULL,
      delivery TEXT, -- enum: steer, queue
      status TEXT NOT NULL, -- enum: pending, promoted, failed
      created_at TIMESTAMP NOT NULL
  );
  ```

## 2. Runner (SessionRunner)
現在の `AgentExecutor` を拡張し、Session の状態機械を管理する `SessionRunner` を作成する。
- 役割: `SessionInbox` を監視し、`PromptAdmitted` イベントを基に LLM リクエストを構築・実行する。
- 状態管理: `SessionState` (running, idle, interrupted) を保持し、プロセス停止時の状態を `SessionStore` に永続化する。
- 実行フロー: `SessionExecution.resume` -> `SessionStore.get` -> `SessionRunner.run`

## 3. 実装のステップ
1. `opencode-storage` に `session_input` テーブルを追加する。
2. `opencode-session` に `Runner` トレイトまたは構造体を定義し、Admission Inbox と統合する。
3. `opencode-agent` の `AgentExecutor` を Runner が利用する形にリファクタリングする。
