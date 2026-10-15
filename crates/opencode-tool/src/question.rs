use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

use crate::{QuestionDef, Tool, ToolContext, ToolError, ToolResult};

pub struct QuestionTool;

impl QuestionTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct QuestionInput {
    #[serde(rename = "questions")]
    questions: Vec<QuestionDef>,
}

#[derive(Debug, Serialize, Deserialize)]
struct QuestionResponse {
    answers: Vec<Vec<String>>,
}

#[async_trait]
impl Tool for QuestionTool {
    fn id(&self) -> &str {
        "question"
    }

    fn description(&self) -> &str {
        "Ask the user clarifying questions during execution. Use to gather preferences, clarify ambiguous requests, or get decisions on implementation choices."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The complete question to ask"
                            },
                            "header": {
                                "type": "string",
                                "description": "Short label for the question (max 30 chars)"
                            },
                            "multiple": {
                                "type": "boolean",
                                "default": false,
                                "description": "Allow selecting multiple options"
                            },
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"}
                                    },
                                    "required": ["label"]
                                },
                                "description": "Available choices for the user"
                            }
                        },
                        "required": ["question"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let input: QuestionInput =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if input.questions.is_empty() {
            return Err(ToolError::InvalidArguments(
                "At least one question is required".to_string(),
            ));
        }

        // Server/TUI executions inject a question callback so the request can be
        // answered through the HTTP API. Direct CLI executions fall back to stdin.
        let answers = if ctx.ask_question.is_some() {
            ctx.question(input.questions.clone()).await?
        } else {
            let mut collected = Vec::with_capacity(input.questions.len());
            for question in &input.questions {
                collected.push(ask_question_via_stdin(question)?);
            }
            collected
        };

        let response = QuestionResponse { answers };

        let output = serde_json::to_string_pretty(&response)
            .unwrap_or_else(|_| format!("{:?}", response.answers));

        Ok(ToolResult {
            title: "User response received".to_string(),
            output,
            metadata: std::collections::HashMap::new(),
            truncated: false,
        })
    }
}

fn ask_question_via_stdin(q: &QuestionDef) -> Result<Vec<String>, ToolError> {
    println!();

    if let Some(ref header) = q.header {
        println!("┌─ {} ─────────────────", header);
    } else {
        println!("┌─ Question ─────────────────");
    }
    println!("│");
    println!("│ {}", q.question);
    println!("│");

    if q.options.is_empty() {
        println!("└─ Type your answer: ");
        print!("> ");
        io::stdout()
            .flush()
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

        let stdin = io::stdin();
        let mut answer = String::new();
        stdin
            .lock()
            .read_line(&mut answer)
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

        return Ok(vec![answer.trim().to_string()]);
    }

    println!("│ Options:");
    for (i, opt) in q.options.iter().enumerate() {
        let num = i + 1;
        if let Some(ref desc) = opt.description {
            println!("│   {}. {} - {}", num, opt.label, desc);
        } else {
            println!("│   {}. {}", num, opt.label);
        }
    }
    println!("│");

    if q.multiple {
        println!("└─ Enter choices (comma-separated, e.g., 1,3): ");
    } else {
        println!("└─ Enter your choice: ");
    }

    print!("> ");
    io::stdout()
        .flush()
        .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

    let stdin = io::stdin();
    let mut input = String::new();
    stdin
        .lock()
        .read_line(&mut input)
        .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

    let input = input.trim();
    let answers: Vec<String> = input
        .split(',')
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }

            if let Ok(num) = s.parse::<usize>() {
                if num > 0 && num <= q.options.len() {
                    return Some(q.options[num - 1].label.clone());
                }
            }

            Some(s.to_string())
        })
        .collect();

    if answers.is_empty() && !q.options.is_empty() {
        return Ok(vec![q.options[0].label.clone()]);
    }

    Ok(answers)
}

impl Default for QuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question_args() -> serde_json::Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Switch to the plan agent?",
                    "header": "Plan Mode",
                    "options": [
                        {"label": "Yes", "description": "Switch to plan agent"},
                        {"label": "No", "description": "Stay with build agent"}
                    ],
                    "multiple": false
                }
            ]
        })
    }

    fn context_with_callback<F, Fut>(callback: F) -> ToolContext
    where
        F: Fn(Vec<QuestionDef>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Vec<Vec<String>>, ToolError>> + Send + 'static,
    {
        ToolContext::new(
            "ses_test".to_string(),
            "msg_test".to_string(),
            ".".to_string(),
        )
        .with_ask_question(callback)
    }

    #[tokio::test]
    async fn execute_uses_configured_question_callback() {
        let ctx = context_with_callback(|questions: Vec<QuestionDef>| async move {
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].question, "Switch to the plan agent?");
            assert_eq!(questions[0].options[1].label, "No");
            Ok(vec![vec!["Yes".to_string()]])
        });

        let result = QuestionTool::new()
            .execute(question_args(), ctx)
            .await
            .expect("question tool should succeed");

        let parsed: QuestionResponse =
            serde_json::from_str(&result.output).expect("output should be valid JSON");
        assert_eq!(parsed.answers, vec![vec!["Yes".to_string()]]);
        assert_eq!(result.title, "User response received");
    }

    #[tokio::test]
    async fn execute_propagates_question_rejection() {
        let ctx = context_with_callback(|_questions: Vec<QuestionDef>| async move {
            Err(ToolError::QuestionRejected(
                "User rejected the question".to_string(),
            ))
        });

        let error = QuestionTool::new()
            .execute(question_args(), ctx)
            .await
            .expect_err("rejection should surface as an error");

        assert!(matches!(error, ToolError::QuestionRejected(_)));
    }

    #[tokio::test]
    async fn execute_rejects_empty_question_list() {
        let ctx = context_with_callback(|_questions: Vec<QuestionDef>| async move {
            Ok(vec![vec!["unexpected".to_string()]])
        });

        let error = QuestionTool::new()
            .execute(serde_json::json!({ "questions": [] }), ctx)
            .await
            .expect_err("empty question list should be rejected");

        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn execute_rejects_malformed_arguments() {
        let ctx = context_with_callback(|_questions: Vec<QuestionDef>| async move {
            Ok(vec![vec!["unexpected".to_string()]])
        });

        let error = QuestionTool::new()
            .execute(serde_json::json!({ "questions": "not-an-array" }), ctx)
            .await
            .expect_err("malformed arguments should be rejected");

        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }
}
