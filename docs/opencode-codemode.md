---
type: Reference
title: "opencode-codemode"
description: "upstream互換の制限付きinterpreterに向けたparser基盤と現状の境界を説明する。"
status: Current
tags:
  - codemode
  - parser
  - crate
verified:
  by: opencode
  at: "2026-10-18T14:27:00+09:00"
---

## opencode-codemode

`opencode-codemode` は upstream CodeMode の制限付きinterpreterを移植する
ためのcrateである。現段階では Oxc parser による構文解析と構文診断のみを
提供し、コード評価やtool呼び出しは行わない。

## Parser boundary

- Oxc `0.154.0` を使い、TypeScriptを含むmodule grammarでparseする。
- upstream同様、トップレベルの `return` と `await` を許可する。
- 構文エラーを `ParseError` として返す。
- 解析ASTのallocatorとsourceは、AST利用中に生存させる必要がある。

## 未実装

- TypeScriptの型消去・transpile parity
- ASTの対応構文検査とinterpreter
- 値モデル、stdlib、tool runtime
- timeout、tool-call数、出力byte数の制限
- upstream parity test suite

Parserが受理する構文は、そのまま実行可能な構文を意味しない。実行器は
後続実装で許可構文を明示し、未対応構文を拒否する。

## Validation

```bash
cargo test -p opencode-codemode
```

## References

- [upstream CodeMode](https://github.com/anomalyco/opencode/tree/941e71dbbb/packages/codemode)
- [Oxc parser](https://github.com/oxc-project/oxc/tree/main/crates/oxc_parser)
