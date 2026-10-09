---
type: ADR
title: プロンプト経路へのツール・システムプロンプト注入
status: Approved
date: 2026-01-15
deciders:
  - OpenCodeRust Team
tags:
  - prompt
  - agent
  - tool
  - system-prompt
---

# ADR-0003: プロンプト経路へのツール・システムプロンプト注入

## Context

`session_prompt` エンドポイントで以下が未実装だった：
- エージェント設定に基づくシステムプロンプト生成
- 利用可能ツール定義のプロバイダへの注入
- `AgentParams`（max_tokens/temperature/top_p）のエージェント設定反映
- `variant` の伝播

## Decision

### 1. システムプロンプト生成（`routes.rs:session_prompt`）
```rust
// AgentRegistry から設定適用済みエージェント取得
let agent_registry = AgentRegistry::from_config(&config);
let agent_info = agent_registry.get(agent_name).cloned().unwrap_or_else(|| AgentInfo::from_builtin(BuiltinAgent::Build));

// 3層構成で結合
let provider_specific = SystemPrompt::for_model(&model_id);      // anthropic/gemini/gpt-5 等
let agent_specific = agent_info.system_prompt.unwrap_or_default(); // エージェント固有
let environment = SystemPrompt::environment(&EnvironmentContext::from_current(...)); // 作業環境

let system_prompt = if agent_specific.is_empty() {
    format!("{}\n\n{}", provider_specific, environment)
} else {
    format!("{}\n\n{}\n\n{}", provider_specific, agent_specific, environment)
};
```

### 2. ツール定義注入
```rust
let tool_registry = create_default_registry().await;
let tool_definitions = tool_registry.list_schemas().await;
let tools: Vec<ToolDefinition> = tool_definitions.into_iter().map(|t| ToolDefinition {
    name: t.name,
    description: Some(t.description),
    parameters: t.parameters,
}).collect();

// 2経路で渡す（互換性・将来拡張）
input.tools = Some(tools.iter().map(|t| (t.name.clone(), true)).collect());
// および
prompt_runner.prompt_with_update_hook(..., tools, ...)
```

### 3. AgentParams 反映
```rust
let agent_params = AgentParams {
    max_tokens: agent_info.max_tokens,
    temperature: agent_info.temperature,
    top_p: agent_info.top_p,
};
```

### 4. variant 伝播
```rust
input.variant = task_variant.clone(); // リクエストの variant をそのまま伝播
```

## Consequences

- エージェント固有の振る舞い（plan=読み取り専用、explore=検索特化等）がプロンプトに反映
- ツール一覧がモデルに渡され、Function Calling が正常動作
- モデルパラメータがエージェント設定準拠に
- variant（reasoning effort 等）がプロバイダに伝達

## Links

- [ADR-0000: プロジェクト憲章](20260115-rust-reimplementation-charter.md)
- 実装: `routes.rs:1762-1836`
- `opencode-agent`: `AgentRegistry`, `AgentInfo`, `BuiltinAgent`
- `opencode-session`: `SystemPrompt`, `AgentParams`, `PromptInput`