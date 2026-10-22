---
type: Reference
title: "opencode-codemode"
description: "upstream互換の制限付きinterpreterに向けたparser・構文検査基盤と現状の境界を説明する。"
status: Current
tags:
  - codemode
  - parser
  - crate
verified:
  by: opencode
  at: "2026-10-22T00:00:00+09:00"
---

## opencode-codemode

`opencode-codemode` は upstream CodeMode の制限付きinterpreterを移植する
ためのcrateである。現段階では Oxc parser による構文解析、構文診断、および
対応構文の静的検査を提供し、コード評価やtool呼び出しは行わない。

## Parser boundary

- Oxc `0.154.0` を使い、TypeScriptを含むmodule grammarでparseする。
- upstream同様、トップレベルの `return` と `await` を許可する。
- 括弧式はASTに残さず、upstreamのAcorn出力と同じ木形状にする。
- 構文エラーを `ParseError` として返す。
- 解析ASTのallocatorとsourceは、AST利用中に生存させる必要がある。

## Supported syntax inspection

`inspect_support` は、upstream interpreterの `evaluateStatement` /
`evaluateExpression` / `declarePattern` が扱う集合を明示的に検査する。

- 対応文: block、break/continue (labelなし)、do-while、empty、expression、
  for、for-in、for-of、if、return、switch、throw、try、while、
  var/let/const、function宣言。
- 対応式: literal、identifier、template literal、array/object、arrow/function、
  assignment、await、binary/logical/unary、call、chain、conditional、
  new (calleeはidentifier)、member、update、spread。
- 対応pattern: identifier、object/array destructuring、default、rest。
- TypeScriptの型消去対象 (`interface`、`type`、`import type`、
  `declare`、`as`、`satisfies`、non-null assertion、型引数) は透過扱い。
- class、generator、`this`、label、sequence、tagged template、import/export、
  destructuring assignment、private field は未対応として報告する。

upstreamは未対応構文を評価到達時に拒否する。`inspect_support` は同じ集合を
静的に報告する補助であり、実行可否を保証しない。値のcoercion、禁止メンバー
(`__proto__` など)、tool権限は別の実行時境界で扱う。

## Value boundary

`value::Value` はデータ専用の値モデルである。

- `undefined`、`null`、boolean、number、string、array、null-prototype object。
- objectは挿入順を保持する。`__proto__`、`constructor`、`prototype` は
  境界で拒否する。
- 深さ上限は upstream と同じ32で、超過は `DataError` として返す。
- 境界通過時、`undefined` と非有限数は `null` になる。
- 関数、promise、tool参照、sandbox classはデータ値ではない。評価器側の
  実行時値として扱い、この境界を通さない。

## Execution limits

`limits` は upstream の3つの任意制限を実装する。

- `timeoutMs`: 1以上。未指定なら無制限。
- `maxToolCalls`: 0以上。超過は `ToolCallLimitExceeded`。
- `maxOutputBytes`: 0以上。値 → logの順に予算を消費し、UTF-8境界を
  壊さず切り詰める。切り詰めは実行失敗にしない。

## Interpreter baseline

`interpreter::execute` は対応構文のうち、tool呼び出しとstdlibを除く範囲を
同期評価する。

- 文: 変数宣言、式、block、if、while/do-while/for/for-in/for-of、
  break/continue、return、switch (fallthrough)、throw/try/catch/finally、
  function宣言 (hoist)。
- 式: literal、identifier、template literal、array/object (spread)、
  arrow/function、assignment (compound/logical)、await (通過)、
  binary/logical/unary、call、conditional、member、optional chain、update。
- 配列とobjectは共有参照で、代入はaliasし、`===` は参照同一性で比較する。
- `typeof` は未宣言識別子に `"undefined"` を返す。
- `await` は値をそのまま返す。promise導入時に解決器へ置き換える。
- 未対応: tool呼び出し、promise、stdlib、`new`、regexp/bigint literal、
  `void`/`delete`/`instanceof`、methodの値としての抽出。
- 緩い等価 (`==`) は基本型のcoercionのみで、objectのToPrimitiveは未実装。

## 未実装

- TypeScriptの型消去・transpile parity
- stdlib、tool runtime、promise
- 実時間timeoutの強制
- upstream parity test suite

## Validation

```bash
cargo test -p opencode-codemode
```

## References

- [upstream CodeMode](https://github.com/anomalyco/opencode/tree/941e71dbbb/packages/codemode)
- [Oxc parser](https://github.com/oxc-project/oxc/tree/main/crates/oxc_parser)
