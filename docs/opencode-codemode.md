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
- 境界通過時、`undefined` と非有限数は `null` になる。整数値は整数として
  直列化し、`-0` は `0` にする (`JSON.stringify` と同じ表記)。
- 関数、promise、tool参照、sandbox classはデータ値ではない。評価器側の
  実行時値として扱い、この境界を通さない。

## Execution limits

`limits` は upstream の3つの任意制限を実装する。

- `timeoutMs`: 1以上。未指定なら無制限。文・block境界で期限を確認する。
  同期hostの呼び出し中は中断できない。
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
- 文字列連結時のobjectは `[object Object]`、配列は要素のカンマ結合になる。
  `+` と関係比較はoperandをcoerceしてから評価し、NaNを含む関係比較は
  すべてfalseになる。`Date` は `+` でISO文字列、関係比較と `==` では
  時刻値になる。`==` は同じ型のobject同士なら参照同一性で比較する。
- 返り値はデータ境界で検証し、関数・tool参照・循環・深さ超過を
  `InvalidDataValue` として拒否する。
- `typeof` は未宣言識別子に `"undefined"` を返す。
- `await` はfulfilled値を取り出し、rejected理由をthrowする。tool呼び出しは
  同期hostで即時に解決する。
- 配列・文字列・数値のmethodは値として取り出せる (`const map = [].map`)。
- 関数呼び出しの深さと文・式の評価深さは、それぞれ64で `RangeError` にする。
  Rustの評価frameがJavaScriptより大きいための固定上限である。
- `new` は `Error`、`TypeError`、`RangeError`、`SyntaxError`、
  `ReferenceError`、`EvalError`、`URIError` のconstructorと
  `Map` / `Set` / `Date` を許可する。
  `Error(...)` は `new` なしでも同じ値を作り、`Map` / `Set` / `Date` は
  `new` なしでは `TypeError` にする。
- `instanceof` はError brandと `Map` / `Set` / `Date` / `Array` / `Object` /
  `Promise` を判定する。`Number` / `String` / `Boolean` は常に `false`
  (boxed値を作らないため)、未知の右辺は `TypeError` にする。
- Error値は `name` と `message` を持ち、`{ name, message }` として直列化する。
  `Object.keys` / `Object.values` / `Object.entries` / `for...in` でも
  `name`、`message` の順に列挙する。
  `Object.keys` は `["name", "message"]` を返し、spreadするとbrandを失う。
- 捕捉された実行時失敗は対応するbrandのError値になる。
- 未対応: regexp/bigint literal、`void`/`delete`、Error/Map/Set/Date以外の
  `new`。
- 緩い等価 (`==`) はToPrimitiveを使い、Dateは時刻値、その他のobjectは
  文字列化で比較する。object同士は参照同一性で判定する。
- `+` はToPrimitive、関係比較と `==` はDateを時刻値として扱う。

## Stdlib baseline

`install_globals` は `console`、`Math`、`JSON`、`Object`、`Array`、`String`、
`Number`、`Boolean`、`Promise`、`Map`、`Set`、`Date`、`parseInt`、
`parseFloat`、`isNaN`、`isFinite`、`encodeURI`、`encodeURIComponent`、
`decodeURI`、`decodeURIComponent`、`undefined`、`NaN`、`Infinity` を登録する。
builtinsはプログラムの親scopeに置くため、`const Date = 1` のような同名宣言で
shadowingできる。

- `console.log/info/warn/error/debug` は引数を空白で連結し、interpreterの
  `logs()` に保存する。`warn` / `error` / `debug` はそれぞれ `[warn]`、
  `[error]`、`[debug]` と空白を前置する。
- consoleは境界JSONではなくdebugger形式で値を描画する。DateはISO、Map/Setは
  `Map(n) [...]` / `Set(n) [...]`、関数とtool参照は `[CodeMode reference]`、
  循環は `[Circular]`、深さ超過は `...` になる。
- `console.dir` は最初の引数のみを描画する。`console.table` はplain objectの
  配列を `(index)\t...` 形式に整形する。
- `Math`: abs、cbrt、ceil、floor、round、trunc、max、min、pow、sqrt、hypot、
  sign、log、log2、log10、exp、sin、cos、tan、PI、E、LN2、LN10、LOG2E、
  LOG10E、SQRT2、SQRT1_2。`round` は `0 < x < 0.5` を `+0` にし、
  `0.49999999999999994` の二重丸めを避ける。負のゼロは保持する。
  `pow` は `(±1) ** ±Infinity` を `NaN`、`(-Infinity) ** 奇数` を
  `-Infinity` にするなどECMAScriptの特殊caseに従う。
- `JSON`: parse、stringify。objectのkey順はECMAScriptと同じく配列indexの
  昇順、その後に挿入順で、parseでも入力順を保つ。`f64` を超える数値literal
  (`1e400` など) は `Infinity` / `-Infinity` としてparseする。stringifyは
  `undefined` をobjectから省略、arrayでは `null` にする。関数・callable・
  未await Promiseは `InvalidDataValue`、循環参照と深すぎる値は `TypeError`
  にする。第3引数のindent (数値0..10または先頭10文字の文字列) を受け付ける。
  replacerの配列・関数は `UnsupportedSyntax` にする。Errorはindent指定時も
  整形する。
- `Number` statics: `Number.isNaN`、`isFinite`、`isInteger`、`isSafeInteger`、
  `parseInt`、`parseFloat`、定数 (`MAX_SAFE_INTEGER`、`MAX_VALUE`、
  `MIN_VALUE` など)。`MIN_VALUE` は最小の正のsubnormal (`5e-324`) で、
  `f64::MIN_POSITIVE` ではない。
- `String` statics: `String.fromCharCode`、`String.fromCodePoint`。
- URI helpers: `encodeURI` / `encodeURIComponent` はUTF-8をpercent-encodeし、
  `decodeURI` / `decodeURIComponent` は厳密なUTF-8として復号する。
  `decodeURI` はreserved文字のescapeを元の表記のまま残す。壊れた `%XX` と
  不正なUTF-8は `URIError` をthrowする。入力はdata境界を通すため、
  callableと未await Promiseは `InvalidDataValue` になる。
- グローバルな `String` / `Number` / `Boolean` / `parseInt` / `parseFloat` は、
  callableと未await Promiseをdata境界で `InvalidDataValue` にする。
  Date/Map/Set/Errorはdataとしてcoercionする。
- `Object`: keys、values、entries、hasOwn、fromEntries (配列とMap)、assign。
  `keys` はdata objectと配列を受け、Date/Map/Set/Promiseでは空配列を返す。
  `values` / `entries` / `hasOwn` は配列とprimitiveを `TypeError` にし、
  Date/Map/Set/Promiseでは `values` / `entries` が空配列、`hasOwn` が
  `false` を返す。`assign` は全引数をsourceとして新しいobjectを返し、
  Date/Map/Setは無視、未await Promiseは拒否する。object spreadも同様に
  Date/Map/Setを無視し、配列と未await Promiseを拒否する。fromEntriesのkeyは
  `String(...)` でcoercionする。列挙は
  ECMAScriptのproperty順 (配列indexの昇順、その後に挿入順) で行い、
  `Object.keys` / `values` / `entries`、`for...in`、`JSON.stringify`、
  spread、`Object.assign` に適用する。`for...in` はplain object、配列、
  `tools` 参照だけを受け、文字列・Map/Set・数値・nullは `TypeError` にする。
- `Array`: isArray、of、from (array-like含む)。`from` は配列・文字列・
  Map/Set・数値lengthを持つobjectを変換し、それ以外 (数値、null、
  lengthのないobjectなど) は空配列を返す。array-likeのlengthが上限を
  超える場合は `TypeError` にする。第2引数のmap関数はupstreamに合わせて
  `UnsupportedSyntax` にする。インスタンスmethodは map、filter、forEach、
  reduce、reduceRight、find、findIndex、findLast、findLastIndex、some、
  every、join、slice、includes、indexOf、lastIndexOf、push、pop、shift、
  unshift、reverse、concat、flat、flatMap、sort、toSorted、toReversed、
  with、at、splice、fill、copyWithin、keys、values、entries。
  `keys` / `values` / `entries` はiteratorではなく配列を返す。
  index系の引数 (`includes` のstart、`slice`、`at`、`with`、`splice`、
  `fill`、`copyWithin`、`flat` のdepthなど) はupstreamと同じく
  「省略またはnumber」だけを受け、文字列・nullなどは `TypeError` にする。
  `join` は引数なしまたは文字列1個だけを受け、`splice()` は何も変更しない。
  callbackの第3引数は呼び出し時snapshotの配列で、receiver自身ではない。
  `sort` / `toSorted` のcomparatorは `undefined` かユーザー関数だけを受け、
  builtin関数は `TypeError` にする。push/unshift/splice/fillはreceiverを
  含む値の挿入を `InvalidDataValue` で拒否する。
  `includes` はSameValueZero、`indexOf` / `lastIndexOf` はstrict equalityで
  比較する。`sort` / `toSorted` の既定比較は `String(...)` のUTF-16 code
  unit順で、`undefined` は末尾に置く。`reverse` と `sort` はupstreamと同じく
  receiverを変更せずコピーを返す (`toReversed` / `toSorted` と同じ)。
  push/pop/shift/unshift/splice/fill/copyWithinはin-placeで変更する。
- `String` instance methods: split、slice、substring、substr、indexOf、lastIndexOf、
  includes、startsWith、endsWith、trim系 (trimLeft/trimRight含む)、
  case変換、charAt、charCodeAt、codePointAt、at、padStart、padEnd、repeat、
  replace、replaceAll、concat、toString、localeCompare。`replace` 系は
  `$$` / `$&` / ``$` `` / `$'` のreplacement patternを展開し、callableの
  replacerは `(match, offset, input)` で呼ぶ。offsetはUTF-16 code unitで
  数える。searchはstringのみを受け、それ以外は `TypeError` にする。
  `lastIndexOf` のpositionは絶対位置としてclampし、負数は0、`NaN` は
  文字列長として扱う。`indexOf` / `includes` / `startsWith` / `endsWith` も
  絶対位置で、負数は0になる。Arrayの検索系 (`indexOf` / `includes` /
  `lastIndexOf`) は負数を末尾からの相対として扱う。
  `split` のlimitはToUint32で変換し、`NaN` / `Infinity` は0、負数は
  2^32のmoduloになる。trim系はBOM (U+FEFF) も除去し、`<` / `>` の
  文字列比較はUTF-16 code unit順になる。
  `normalize` とRegExp依存のmatch系は未実装。
- `Number` instance methods: toFixed、toPrecision、toExponential、toString
  (radix 2..36の整数)、valueOf。digitsが範囲外なら `RangeError` にする。
  丸めはECMAScriptと同じround-half-away (0.5は0から遠い側) で行う。
  `toPrecision` / `toExponential` は最短表記ではなく正確な10進展開から
  指数を決めるため、`(1e-7).toPrecision(17)` は `9.9999999999999995e-8`
  になる。
- 数値の文字列化はECMAScriptの `Number::toString` に合わせ、指数が -6 未満または
  20 超のとき指数表記 (`1e+21`、`1e-7`) を使う。
- `Map`: `new Map(entries | Map)`、get、set、has、delete、clear、forEach、
  keys、values、entries、size。keyはSameValueZeroで比較し、`NaN` は同一key、
  objectは参照同一性で区別する。`keys` / `values` / `entries` は配列を返す。
- `Set`: `new Set(array | Set | string)`、add、has、delete、clear、forEach、
  keys、values、entries、size。membershipはSameValueZero。
- Map/Setはspreadとfor-ofで反復でき、Mapは `[key, value]`、Setは値を返す。
  実データ境界では `{}` に直列化する。
- `Date`: `new Date()`、`new Date(ms)`、`new Date(isoString)`、
  `new Date(year, month, ...)`、`Date.now`、`Date.parse`、`Date.UTC`。
  `getTime` / `valueOf`、ISO getter、local getter、`toISOString`、`toJSON`、
  `toString`、`getTimezoneOffset` を持つ。
- DateはUTC固定で評価する。component constructorとlocal getterもUTCとして
  扱い、`getTimezoneOffset()` は有効日付で `0` を返す。これはhost timezone
  非依存を優先した意図的な差分である。
- `Date.parse` はISO 8601のsubset (`YYYY`、`YYYY-MM`、`YYYY-MM-DD`、
  符号付き拡張年 `-000001` / `+010000`、`T`またはspace区切り、
  `Z` / `±HH:MM` / `±HHMM`) を受理し、zone省略時はUTCとする。
  `24:00:00.000` は翌日として受理するが、hour 24で分秒以降が非ゼロなら
  `NaN`。月日の範囲外はrolloverし、月0・日0・不正なoffsetは `NaN`。
- 文字列化と境界直列化はISO形式、無効日付は `toString()` が
  `Invalid Date`、境界では `null` になる。`toISOString()` は無効日付で
  `RangeError` を投げる。

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
  `Promise.race` は同期的に評価する。`await` はfulfilled値を取り出し、
  rejected理由をthrowする。空の `Promise.race` はエラーにする。
- `.then` / `.catch` / `.finally` と、未await Promiseのproperty読み取りは
  awaitを促す診断を返す。
- 未await Promiseは、境界に加えてbinary operatorと `JSON.stringify` でも
  `InvalidDataValue` として拒否する。`typeof` と `instanceof` はcoercion
  しないため、そのまま観察できる。
- 真の非同期実行、tool呼び出しの同時実行制御、未await rejectionの診断は
  未実装。

## High-level execution API

`execute_code(code, host, limits)` は upstream `CodeMode.execute` に相当する
入口で、parse、評価、limit適用、data境界の正規化をまとめて行う。

- 成功は `CodeModeResult::Success` で、`value` (JSON)、`logs`、`truncated`、
  `tool_calls` を返す。`value` は `to_json` で正規化し、`NaN` / `Infinity` は
  `null` になる。
- 失敗は `CodeModeResult::Failure` で、`Diagnostic` と失敗までの `logs`、
  `tool_calls` を返す。Rustの `Result` ではなくデータとして扱う。
- `DiagnosticKind` は `ParseError`、`UnsupportedSyntax`、`InvalidDataValue`、
  `ToolCallLimitExceeded`、`TimeoutExceeded`、`ToolFailure`、
  `ExecutionFailure` などを区別する。
- `Location` は1-basedのlineとcolumnで、runtime errorのspanから算出する。
  parse errorは位置を持たない。
- `max_output_bytes` は値とlogsに適用し、超過時は `truncated` をtrueにして
  upstreamと同じmarker文字列へ置き換える。
- `tool_calls` は `maxToolCalls` の有無にかかわらず、admitされた順に記録する。

## 未実装

- RegExp、URL、`String.normalize`
- TypeScriptの型消去・transpile parity
- 非同期tool実行と未await rejectionの診断
- 同期host呼び出しを中断するtimeout
- upstream parity test suiteの完全移植 (依存不要な差分コーパスは実装済み)

## Validation

```bash
cargo test -p opencode-codemode
markdownlint docs/opencode-codemode.md
```

`tests/parity_corpus.rs` は依存不要なstdlib・coercion・operatorの差分
コーパス (`tests/parity_cases.json`) を実行する。期待値はNode.jsで生成し、
UTC固定Date・plain-object Errorなどupstream固有の差分をレビューして
除外している。

## References

- [upstream CodeMode](https://github.com/anomalyco/opencode/tree/941e71dbbb/packages/codemode)
- [Oxc parser](https://github.com/oxc-project/oxc/tree/main/crates/oxc_parser)
