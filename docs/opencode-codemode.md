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
ためのcrateである。Oxc parser による構文解析と構文診断、対応構文の静的検査、
データ値境界、実行制限、同期評価器、stdlibの一部、同期のtool境界を提供する。
非同期実行と実時間timeoutは未実装である。

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

`interpreter::execute` は対応構文のうち、実時間timeoutを除く範囲を
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
- 配列・文字列・数値のmethodは値として取り出せる (`const map = [].map`)。
- 未対応: tool呼び出し、promise、`new`、regexp/bigint literal、
  `void`/`delete`/`instanceof`。
- 緩い等価 (`==`) は基本型のcoercionのみで、objectのToPrimitiveは未実装。

## Stdlib baseline

`install_globals` は `console`、`Math`、`JSON`、`Object`、`Array`、`String`、
`Number`、`Boolean`、`parseInt`、`parseFloat`、`isNaN`、`isFinite` を登録する。

- `console.log/info/warn/error` は引数を空白で連結し、interpreterの
  `logs()` に保存する。出力先は呼び出し側が決める。
- `Math`: abs、ceil、floor、round、trunc、max、min、pow、sqrt、sign、
  log、exp、sin、cos、tan、PI、E。
- `JSON`: parse、stringify。stringifyは挿入順を保持し、関数と `undefined` は
  objectから省略、arrayでは `null` にする。循環参照と深い値は `null` に
  落として停止する。
- `Object`: keys、values、entries、fromEntries、assign。
- `Array`: isArray、from。インスタンスmethodは map、filter、forEach、
  reduce、reduceRight、find、findIndex、some、every、join、slice、
  includes、indexOf、push、pop、shift、unshift、reverse、concat、flat、
  sort、at。
- `String`: split、slice、substring、substr、indexOf、lastIndexOf、
  includes、startsWith、endsWith、trim系、case変換、charAt、charCodeAt、
  at、padStart、padEnd、repeat、replace、concat。
- `Number`: toFixed、toString (radix 2..36の整数)、valueOf。

文字列のindexはUTF-16 code unitではなくUnicode scalar値で数える。
`toUpperCase` / `toLowerCase` はRustのUnicode変換を使う。callbackを取る
配列methodは反復開始時に要素をsnapshotする。

## Tool boundary

`tools` は `ToolHost` に解決される参照で、`tools.ns.tool` 形式のパスを
構築する。呼び出しは同期で、引数と結果はplain data境界を通る。

- `Object.keys(tools)` / `Object.keys(tools.ns)` はhostの名前を列挙する。
- 引数は深さ32、循環、関数、禁止メンバーを拒否する。
- 結果は同じ境界を通す。hostの失敗は `message` と任意の `data` を持つ
  objectとしてthrowし、`try`/`catch` で捕捉できる。
- `maxToolCalls` は `set_limits` / `set_tool_call_limit` で適用し、超過は
  `ToolCallLimitExceeded` を返す。
- `Promise.resolve`、`Promise.reject`、`Promise.all`、`Promise.allSettled`、
  `Promise.race` は同期的に評価する。`await` は値を通す。
- 真の非同期実行、tool呼び出しの同時実行制御、未await rejectionの診断は
  未実装。

## 未実装

- TypeScriptの型消去・transpile parity
- 非同期tool実行と未await rejectionの診断
- 実時間timeoutの強制
- upstream parity test suite

## Validation

```bash
cargo test -p opencode-codemode
```

## References

- [upstream CodeMode](https://github.com/anomalyco/opencode/tree/941e71dbbb/packages/codemode)
- [Oxc parser](https://github.com/oxc-project/oxc/tree/main/crates/oxc_parser)
