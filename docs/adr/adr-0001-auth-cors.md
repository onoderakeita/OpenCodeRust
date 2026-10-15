---
type: ADR
title: HTTP認証必須化とCORS強化
status: Approved
date: 2026-01-10
deciders:
  - OpenCodeRust Team
tags:
  - security
  - server
  - auth
---

# ADR-0001: HTTP認証必須化とCORS強化

## Context

OpenCodeRust サーバモード（`opencoderust serve`）はローカル開発用だが、以下のリスクがあった：

- 認証なしで任意のコード実行・ファイル読取り可能
- CORS 設定が緩く、`http://localhost.attacker.com` 等からのアクセス許容
- 本番・共有環境での利用時に重大なセキュリティホール

## Decision

### 1. Bearer トークン認証必須化
- 起動時に 32 バイト乱数トークン生成、ログ出力
- `Authorization: Bearer <token>` ヘッダー必須
- 環境変数 `OPENCODE_NO_AUTH=1` で明示的に無効化可能（開発用のみ）

### 2. CORS 厳格化（Origin 完全一致）
- スキーム・ホスト・ポート完全一致必須
- 許可オリジン:
  - `http://localhost:<port>` / `https://localhost:<port>` / `http://127.0.0.1:<port>` / `https://[::1]:<port>`
  - `tauri://localhost` / `http://tauri.localhost` / `https://tauri.localhost`
  - `https://opencode.ai` / `https://www.opencode.ai`
  - 設定追加分（完全一致）
- `http://localhost.attacker.com` 等のサブドメイン攻撃を遮断

### 3. 許可メソッド・ヘッダー明示
- Methods: `GET, POST, PUT, PATCH, DELETE`
- Headers: `Authorization, Content-Type, Accept`

## Implementation

- `crates/opencode-server/src/server.rs`:
  - `generate_auth_token()`, `is_auth_enabled()`, `auth_middleware()`
  - `cors_layer()` に `AllowOrigin::predicate` で完全一致判定
- `crates/opencode-server/src/routes.rs`:
  - `is_extra_allowed_origin_exact()` 追加

## Consequences

- 既存スクリプトはトークン取得・ヘッダー付与必要
- `OPENCODE_NO_AUTH=1` は非推奨・本番禁止を明記
- Tauri/Web 連携は既存オリジン維持で互換性確保

## Links

- [ADR-0000: プロジェクト憲章](20260115-rust-reimplementation-charter.md)
- 実装: `server.rs:41-63, 558-628, 630-664`