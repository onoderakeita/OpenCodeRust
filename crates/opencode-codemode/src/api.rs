//! High-level execution contract mirroring upstream `CodeMode.execute`.
//!
//! [`execute_code`] parses a program, runs it against an optional host tool
//! tree, applies the configured limits, and returns either a JSON success or a
//! structured diagnostic. Program failures are data, not Rust errors, matching
//! the upstream `Result` union.

use std::fmt::{self, Display, Formatter};

use serde_json::Value as JsonValue;

use crate::interpreter::{Interpreter, RuntimeErrorKind};
use crate::limits::{bound_logs, bound_output, ExecutionLimits};
use crate::parse_program;
use crate::tools::ToolHost;

/// Category of an execution diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// The source could not be parsed.
    ParseError,
    /// Parsed syntax outside the interpreter subset was reached.
    UnsupportedSyntax,
    /// A referenced tool does not exist.
    UnknownTool,
    /// A tool rejected its arguments.
    InvalidToolInput,
    /// A tool returned a value outside the plain-data boundary.
    InvalidToolOutput,
    /// A value violated the plain-data boundary.
    InvalidDataValue,
    /// The configured tool-call budget was exhausted.
    ToolCallLimitExceeded,
    /// The wall-clock timeout elapsed.
    TimeoutExceeded,
    /// A host tool failed.
    ToolFailure,
    /// Any other program failure.
    ExecutionFailure,
}

impl DiagnosticKind {
    /// Stable diagnostic label matching the upstream schema.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "ParseError",
            Self::UnsupportedSyntax => "UnsupportedSyntax",
            Self::UnknownTool => "UnknownTool",
            Self::InvalidToolInput => "InvalidToolInput",
            Self::InvalidToolOutput => "InvalidToolOutput",
            Self::InvalidDataValue => "InvalidDataValue",
            Self::ToolCallLimitExceeded => "ToolCallLimitExceeded",
            Self::TimeoutExceeded => "TimeoutExceeded",
            Self::ToolFailure => "ToolFailure",
            Self::ExecutionFailure => "ExecutionFailure",
        }
    }
}

impl Display for DiagnosticKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One-based source position of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// One-based line number.
    pub line: u32,
    /// One-based column number.
    pub column: u32,
}

/// Structured program failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Failure category.
    pub kind: DiagnosticKind,
    /// Model-facing message.
    pub message: String,
    /// Source position when the parser or interpreter reported one.
    pub location: Option<Location>,
    /// Optional remediation hints.
    pub suggestions: Vec<String>,
}

/// Successful execution after the result crossed the plain-data boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Success {
    /// JSON-normalized result value.
    pub value: JsonValue,
    /// Captured console output.
    pub logs: Vec<String>,
    /// `true` when the value or logs were truncated.
    pub truncated: bool,
    /// Names of admitted tool calls in invocation order.
    pub tool_calls: Vec<String>,
}

/// Failed execution with output collected before the diagnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// Structured diagnostic.
    pub error: Diagnostic,
    /// Captured console output.
    pub logs: Vec<String>,
    /// `true` when the logs were truncated.
    pub truncated: bool,
    /// Names of admitted tool calls in invocation order.
    pub tool_calls: Vec<String>,
}

/// Outcome of [`execute_code`].
#[derive(Debug, Clone, PartialEq)]
pub enum CodeModeResult {
    /// Program completed and its result is plain data.
    Success(Success),
    /// Program failed before a result could be produced.
    Failure(Failure),
}

impl CodeModeResult {
    /// Returns `true` when the program succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    /// Returns the success payload, when present.
    pub fn success(&self) -> Option<&Success> {
        match self {
            Self::Success(success) => Some(success),
            Self::Failure(_) => None,
        }
    }

    /// Returns the failure payload, when present.
    pub fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Failure(failure) => Some(failure),
            Self::Success(_) => None,
        }
    }
}

/// Parses and executes one CodeMode program.
///
/// The program runs with the supplied host (or an empty tool tree) and limits.
/// Parse and runtime failures are returned as [`CodeModeResult::Failure`] so
/// callers can surface diagnostics to the model without handling Rust errors.
pub fn execute_code(
    code: &str,
    host: Option<Box<dyn ToolHost>>,
    limits: &ExecutionLimits,
) -> CodeModeResult {
    let allocator = oxc_allocator::Allocator::default();
    let program = match parse_program(&allocator, code) {
        Ok(program) => program,
        Err(error) => {
            return CodeModeResult::Failure(Failure {
                error: Diagnostic {
                    kind: DiagnosticKind::ParseError,
                    message: error.to_string(),
                    location: None,
                    suggestions: Vec::new(),
                },
                logs: Vec::new(),
                truncated: false,
                tool_calls: Vec::new(),
            });
        }
    };

    let mut interpreter = Interpreter::new();
    if let Some(host) = host {
        interpreter.set_tool_host(host);
    }
    interpreter.set_limits(limits);

    match interpreter.run(&program) {
        Ok(value) => {
            let value = value.to_json();
            let logs = interpreter.logs().to_vec();
            let tool_calls = interpreter.tool_calls().to_vec();

            let Some(max_bytes) = limits.max_output_bytes else {
                return CodeModeResult::Success(Success {
                    value,
                    logs,
                    truncated: false,
                    tool_calls,
                });
            };

            let serialized = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
            let bounded = bound_output(&serialized, &logs, max_bytes);
            let value = serde_json::from_str(&bounded.value)
                .unwrap_or_else(|_| JsonValue::String(bounded.value.clone()));
            CodeModeResult::Success(Success {
                value,
                logs: bounded.logs,
                truncated: bounded.truncated,
                tool_calls,
            })
        }
        Err(error) => {
            let logs = interpreter.logs().to_vec();
            let tool_calls = interpreter.tool_calls().to_vec();
            let (logs, truncated) = match limits.max_output_bytes {
                Some(max_bytes) => {
                    let bounded = bound_logs(&logs, max_bytes);
                    (bounded.lines, bounded.truncated)
                }
                None => (logs, false),
            };

            let kind = map_runtime_kind(error.kind());
            CodeModeResult::Failure(Failure {
                error: Diagnostic {
                    kind,
                    message: error.message().to_owned(),
                    location: source_location(code, error.span().start as usize),
                    suggestions: suggestions(kind),
                },
                logs,
                truncated,
                tool_calls,
            })
        }
    }
}

fn map_runtime_kind(kind: RuntimeErrorKind) -> DiagnosticKind {
    match kind {
        RuntimeErrorKind::UnsupportedSyntax => DiagnosticKind::UnsupportedSyntax,
        RuntimeErrorKind::InvalidDataValue => DiagnosticKind::InvalidDataValue,
        RuntimeErrorKind::ToolCallLimitExceeded => DiagnosticKind::ToolCallLimitExceeded,
        RuntimeErrorKind::Timeout => DiagnosticKind::TimeoutExceeded,
        RuntimeErrorKind::ToolFailure => DiagnosticKind::ToolFailure,
        RuntimeErrorKind::ReferenceError
        | RuntimeErrorKind::TypeError
        | RuntimeErrorKind::RangeError
        | RuntimeErrorKind::SyntaxError
        | RuntimeErrorKind::ExecutionFailure => DiagnosticKind::ExecutionFailure,
    }
}

fn suggestions(kind: DiagnosticKind) -> Vec<String> {
    match kind {
        DiagnosticKind::ToolCallLimitExceeded => {
            vec!["Raise maxToolCalls or reduce the number of tool calls.".to_owned()]
        }
        DiagnosticKind::TimeoutExceeded => {
            vec!["Increase timeoutMs or reduce the work performed by the program.".to_owned()]
        }
        DiagnosticKind::InvalidDataValue => {
            vec![
                "Return plain data rather than functions, promises, or circular values.".to_owned(),
            ]
        }
        DiagnosticKind::ParseError
        | DiagnosticKind::UnsupportedSyntax
        | DiagnosticKind::UnknownTool
        | DiagnosticKind::InvalidToolInput
        | DiagnosticKind::InvalidToolOutput
        | DiagnosticKind::ToolFailure
        | DiagnosticKind::ExecutionFailure => Vec::new(),
    }
}

fn source_location(code: &str, offset: usize) -> Option<Location> {
    if offset > code.len() || !code.is_char_boundary(offset) {
        return None;
    }
    let prefix = &code[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let column_prefix = prefix.rsplit('\n').next().unwrap_or("");
    Some(Location {
        line,
        column: column_prefix.chars().count() as u32 + 1,
    })
}

/// Test-only host that echoes one string argument and records invocations.
#[cfg(test)]
mod tests {
    use serde_json::Value as JsonValue;

    use crate::limits::ExecutionLimits;
    use crate::tools::{ToolError, ToolHost};

    use super::{execute_code, CodeModeResult, DiagnosticKind};

    #[derive(Default)]
    struct EchoHost {
        calls: Vec<String>,
    }

    impl ToolHost for EchoHost {
        fn keys(&self, path: &[String]) -> Vec<String> {
            if path.is_empty() {
                vec!["echo".to_owned()]
            } else {
                Vec::new()
            }
        }

        fn invoke(
            &mut self,
            path: &[String],
            arguments: Vec<JsonValue>,
        ) -> Result<JsonValue, ToolError> {
            self.calls.push(path.join("."));
            Ok(arguments.first().cloned().unwrap_or(JsonValue::Null))
        }
    }

    fn success(result: CodeModeResult) -> super::Success {
        match result {
            CodeModeResult::Success(success) => success,
            CodeModeResult::Failure(failure) => {
                panic!("expected success, got {:?}", failure.error)
            }
        }
    }

    fn failure(result: CodeModeResult) -> super::Failure {
        match result {
            CodeModeResult::Failure(failure) => failure,
            CodeModeResult::Success(success) => panic!("expected failure, got {success:?}"),
        }
    }

    #[test]
    fn returns_value_logs_and_tool_calls() {
        let code = r#"
            console.log("hello");
            const result = await tools.echo({ value: 1 });
            return [result.value, 1 / 0, undefined];
        "#;
        let result = execute_code(
            code,
            Some(Box::new(EchoHost::default())),
            &ExecutionLimits::default(),
        );
        let success = success(result);
        assert_eq!(success.value[0].as_f64(), Some(1.0));
        assert!(success.value[1].is_null());
        assert!(success.value[2].is_null());
        assert_eq!(success.logs, ["hello"]);
        assert!(!success.truncated);
        assert_eq!(success.tool_calls, ["echo"]);
    }

    #[test]
    fn serializes_dates_to_iso_strings() {
        let epoch = success(execute_code(
            "return new Date(0);",
            None,
            &ExecutionLimits::default(),
        ));
        assert_eq!(
            epoch.value,
            JsonValue::String("1970-01-01T00:00:00.000Z".to_owned())
        );

        let nested = success(execute_code(
            "return { when: new Date(1000) };",
            None,
            &ExecutionLimits::default(),
        ));
        assert_eq!(
            nested.value["when"],
            JsonValue::String("1970-01-01T00:00:01.000Z".to_owned())
        );
    }

    #[test]
    fn reports_parse_failures() {
        let failure = failure(execute_code("const = ;", None, &ExecutionLimits::default()));
        assert_eq!(failure.error.kind, DiagnosticKind::ParseError);
        assert!(!failure.error.message.is_empty());
        assert!(failure.error.location.is_none());
        assert!(failure.tool_calls.is_empty());
    }

    #[test]
    fn reports_runtime_failures_with_a_location() {
        let failure = failure(execute_code(
            "console.log(\"before\");\nreturn missing;",
            None,
            &ExecutionLimits::default(),
        ));
        assert_eq!(failure.error.kind, DiagnosticKind::ExecutionFailure);
        assert_eq!(
            failure.error.location,
            Some(super::Location { line: 2, column: 8 })
        );
        assert_eq!(failure.logs, ["before"]);
    }

    #[test]
    fn reports_tool_call_and_timeout_limits() {
        let code = "await tools.echo(1); return 1;";
        let limits = ExecutionLimits {
            max_tool_calls: Some(0),
            ..ExecutionLimits::default()
        };
        let tool_failure = failure(execute_code(
            code,
            Some(Box::new(EchoHost::default())),
            &limits,
        ));
        assert_eq!(
            tool_failure.error.kind,
            DiagnosticKind::ToolCallLimitExceeded
        );
        assert!(!tool_failure.error.suggestions.is_empty());

        let limits = ExecutionLimits {
            timeout_ms: Some(1),
            ..ExecutionLimits::default()
        };
        let timeout = failure(execute_code("while (true) {}", None, &limits));
        assert_eq!(timeout.error.kind, DiagnosticKind::TimeoutExceeded);
        assert!(!timeout.error.suggestions.is_empty());
    }

    #[test]
    fn truncates_oversized_values_and_logs() {
        let limits = ExecutionLimits {
            max_output_bytes: Some(16),
            ..ExecutionLimits::default()
        };
        let success = success(execute_code(
            "console.log(\"a long log line\"); return \"abcdefghijklmnopqrstuvwxyz\";",
            None,
            &limits,
        ));
        assert!(success.truncated);
        let value = success.value.as_str().expect("truncated marker is text");
        assert!(value.contains("result truncated"));
        assert!(success
            .logs
            .iter()
            .any(|line| line.contains("logs truncated")));
    }

    #[test]
    fn rejects_non_data_results() {
        let failure = failure(execute_code(
            "return () => 1;",
            None,
            &ExecutionLimits::default(),
        ));
        assert_eq!(failure.error.kind, DiagnosticKind::InvalidDataValue);
        assert!(!failure.error.suggestions.is_empty());
    }
}
