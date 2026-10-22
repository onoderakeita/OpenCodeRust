//! Confined CodeMode interpreter foundation: parser, syntax inspection, data
//! boundary, execution limits, synchronous evaluator, standard library, and
//! host tool references.

pub mod interpreter;
pub mod limits;
pub mod support;
pub mod tools;
pub mod value;

use std::fmt::{self, Display, Formatter};

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;

pub use interpreter::{execute, execute_with_host, Interpreter, RuntimeError, RuntimeErrorKind};
pub use limits::{
    bound_logs, bound_output, bound_serialized_value, truncate_utf8, BoundedLogs, BoundedOutput,
    ExecutionLimits, LimitError, ToolCallBudget,
};
pub use support::{inspect_support, SupportReport, UnsupportedSyntax};
pub use tools::{ToolError, ToolHost};
pub use value::{check_data, is_blocked_member, DataError, FunctionId, Value, MAX_VALUE_DEPTH};

/// Parse a CodeMode program with TypeScript-capable module grammar and
/// top-level `return` enabled, matching the upstream transpile-and-interpret
/// input path.
///
/// Parenthesized expressions are not preserved so that the tree shape matches
/// the upstream Acorn output.
///
/// The allocator and source must outlive the returned AST.
pub fn parse_program<'a>(
    allocator: &'a Allocator,
    source: &'a str,
) -> Result<Program<'a>, ParseError> {
    let options = ParseOptions {
        allow_return_outside_function: true,
        preserve_parens: false,
        ..ParseOptions::default()
    };
    let parsed = Parser::new(allocator, source, SourceType::ts())
        .with_options(options)
        .parse();

    if parsed.diagnostics.is_empty() && !parsed.fatal_error {
        return Ok(parsed.program);
    }

    Err(ParseError {
        diagnostics: parsed.diagnostics.iter().map(ToString::to_string).collect(),
    })
}

/// Syntax diagnostics produced while parsing a CodeMode program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    diagnostics: Vec<String>,
}

impl ParseError {
    /// Returns the parser diagnostics in source order.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }
}

impl Display for ParseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostics.join("\n"))
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;

    use super::parse_program;

    #[test]
    fn parses_upstream_style_top_level_await_return_and_supported_syntax() {
        let allocator = Allocator::default();
        let source = r#"
            const query: string = `rust ${"parser"}`;
            const { data: records = [] } = await tools.search({ query: `rust ${"parser"}` });
            const ids = records?.map(({ id, ...rest }: { id: string }) => ({ id, ...rest })) ?? [];
            for (const id of ids) {
                await tools.fetch({ id });
            }
            return ids;
        "#;

        let program = parse_program(&allocator, source).expect("program should parse");
        assert_eq!(program.body.len(), 5);
    }

    #[test]
    fn returns_diagnostics_for_invalid_syntax() {
        let allocator = Allocator::default();
        let error = parse_program(&allocator, "const = ;").expect_err("invalid syntax");

        assert!(!error.diagnostics().is_empty());
        assert!(!error.to_string().is_empty());
    }
}
