---
type: Playbook
title: "依存関係セキュリティ監査"
description: "cargo-audit の実行手順 (Termux 含む) と advisory triage の記録。"
status: Current
tags:
  - security
  - dependencies
  - audit
verified:
  by: opencode
  at: "2026-10-18T11:46:00+09:00"
---

## 実行手順

初回のみ `cargo install cargo-audit --locked`。

```bash
cargo audit
```

`.cargo/audit.toml` の ignore は根拠付きで管理する。

### Termux/Android の回避策

advisory DB の自動取得が `rustls-platform-verifier` の Android 初期化を
要求して失敗する (`Expect rustls-platform-verifier to be initialized`)。
system git で clone した DB を `--db` で指定する。

```bash
git clone --depth 1 https://github.com/RustSec/advisory-db.git \
  ~/.cache/rustsec-advisory-db
cargo audit --db ~/.cache/rustsec-advisory-db --no-fetch
```

## triage 記録 (2026-10-18)

対象: Cargo.lock の 468 crate。初回結果は vulnerabilities 5 / warnings 8。

### 対応済み

- `rustls` 0.23.45 → 0.23.46 (RUSTSEC-2026-0346〜0349、reqwest / oauth2
  経由)。
- `atty` 0.2.14 を opencode-tui から削除。未使用の直接依存だった
  (RUSTSEC-2024-0375 unmaintained / RUSTSEC-2021-0145 unsound)。

### ignore (.cargo/audit.toml)

- `rsa` 0.9.10 (RUSTSEC-2023-0071): 修正版なし (patched = [])。
  sqlx-mysql 経由で lock にのみ存在し build graph 外。

### 未対応 warning (記録のみ)

| crate | advisory | 依存元 | 解消条件 |
| --- | --- | --- | --- |
| `lru` 0.12.5 | RUSTSEC-2026-0002 / 0253 | ratatui 0.27 | ratatui 更新待ち |
| `bincode` 1.3.3 | RUSTSEC-2025-0141 | syntect 5.3.0 | syntect 更新待ち |
| `serial` 0.4.0 | RUSTSEC-2017-0008 | portable-pty 0.8 | portable-pty 更新待ち |
| `instant` 0.1.13 | RUSTSEC-2024-0384 | notify v7 | notify 更新待ち |
| `paste` 1.0.15 | RUSTSEC-2024-0436 | ratatui 0.27 | ratatui 更新待ち |

補足: `lru` の修正版は 0.16.3 以降のみで、ratatui 0.27 の `^0.12`
制約では適用できない。ratatui 更新時に再評価する。

## 残作業

- CI への audit ゲート追加 (共通ワークフロー `rust-gates.yml` 側で検討)。
- ratatui / syntect / portable-pty / notify 更新時に warning を再評価。
