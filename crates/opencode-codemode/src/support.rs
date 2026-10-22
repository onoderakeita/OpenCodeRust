//! Static inspection of the upstream-compatible supported syntax subset.
//!
//! The upstream interpreter rejects unsupported nodes lazily, when evaluation
//! reaches them. This module mirrors the same subset as an explicit, inspectable
//! table so that diagnostics and tests can be written without executing code.
//!
//! A successful inspection does **not** mean a program is executable: value
//! coercion, blocked member names, and tool authority are separate runtime
//! concerns. TypeScript-only syntax that the upstream transpile step erases
//! (`interface`, `type`, `as`, `satisfies`, non-null assertions, type arguments)
//! is treated as transparent and is not reported.

use oxc_ast::ast::{
    ArrayExpressionElement, ArrowFunctionBody, ArrowFunctionExpression, AssignmentTarget,
    AssignmentTargetPattern, BindingPattern, Declaration, Expression, ForStatementInit,
    ForStatementLeft, Function, FunctionBody, ImportOrExportKind, ModuleDeclaration,
    ObjectPropertyKind, Program, PropertyKey, SimpleAssignmentTarget, Statement, SwitchStatement,
    VariableDeclaration,
};
use oxc_span::{GetSpan, Span};

/// A syntax node outside the interpreter's supported subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedSyntax {
    kind: &'static str,
    span: Span,
}

impl UnsupportedSyntax {
    /// ESTree-style node name, matching the upstream diagnostic text.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Source span of the first offending node.
    pub fn span(&self) -> Span {
        self.span
    }
}

/// Result of inspecting a parsed program against the supported subset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportReport {
    unsupported: Vec<UnsupportedSyntax>,
}

impl SupportReport {
    /// Returns `true` when no unsupported syntax was found.
    pub fn is_supported(&self) -> bool {
        self.unsupported.is_empty()
    }

    /// Unsupported nodes in source traversal order.
    pub fn unsupported(&self) -> &[UnsupportedSyntax] {
        &self.unsupported
    }

    fn push(&mut self, kind: &'static str, span: Span) {
        self.unsupported.push(UnsupportedSyntax { kind, span });
    }
}

/// Inspect a parsed program against the upstream interpreter's supported
/// statement, expression, and binding-pattern subset.
pub fn inspect_support(program: &Program<'_>) -> SupportReport {
    let mut inspector = Inspector {
        report: SupportReport::default(),
    };
    inspector.statements(&program.body);
    inspector.report
}

struct Inspector {
    report: SupportReport,
}

impl Inspector {
    fn unsupported(&mut self, kind: &'static str, span: Span) {
        self.report.push(kind, span);
    }

    fn statements<'a>(&mut self, statements: &[Statement<'a>]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement<'_>) {
        if let Some(declaration) = statement.as_declaration() {
            self.declaration(declaration);
            return;
        }
        if let Some(module) = statement.as_module_declaration() {
            self.module(module);
            return;
        }

        match statement {
            Statement::BlockStatement(block) => self.statements(&block.body),
            Statement::BreakStatement(branch) => {
                if branch.label.is_some() {
                    self.unsupported("LabeledStatement", branch.span);
                }
            }
            Statement::ContinueStatement(branch) => {
                if branch.label.is_some() {
                    self.unsupported("LabeledStatement", branch.span);
                }
            }
            Statement::DoWhileStatement(statement) => {
                self.statement(&statement.body);
                self.expression(&statement.test);
            }
            Statement::EmptyStatement(_) => {}
            Statement::ExpressionStatement(statement) => self.expression(&statement.expression),
            Statement::ForInStatement(statement) => {
                self.for_left(&statement.left);
                self.expression(&statement.right);
                self.statement(&statement.body);
            }
            Statement::ForOfStatement(statement) => {
                self.for_left(&statement.left);
                self.expression(&statement.right);
                self.statement(&statement.body);
            }
            Statement::ForStatement(statement) => {
                if let Some(init) = &statement.init {
                    self.for_init(init);
                }
                if let Some(test) = &statement.test {
                    self.expression(test);
                }
                if let Some(update) = &statement.update {
                    self.expression(update);
                }
                self.statement(&statement.body);
            }
            Statement::IfStatement(statement) => {
                self.expression(&statement.test);
                self.statement(&statement.consequent);
                if let Some(alternate) = &statement.alternate {
                    self.statement(alternate);
                }
            }
            Statement::ReturnStatement(statement) => {
                if let Some(argument) = &statement.argument {
                    self.expression(argument);
                }
            }
            Statement::SwitchStatement(statement) => self.switch_statement(statement),
            Statement::ThrowStatement(statement) => self.expression(&statement.argument),
            Statement::TryStatement(statement) => {
                self.statements(&statement.block.body);
                if let Some(handler) = &statement.handler {
                    if let Some(parameter) = &handler.param {
                        self.pattern(&parameter.pattern);
                    }
                    self.statements(&handler.body.body);
                }
                if let Some(finalizer) = &statement.finalizer {
                    self.statements(&finalizer.body);
                }
            }
            Statement::WhileStatement(statement) => {
                self.expression(&statement.test);
                self.statement(&statement.body);
            }
            _ => self.unsupported(statement_kind(statement), statement.span()),
        }
    }

    fn switch_statement(&mut self, statement: &SwitchStatement<'_>) {
        self.expression(&statement.discriminant);
        for case in &statement.cases {
            if let Some(test) = &case.test {
                self.expression(test);
            }
            self.statements(&case.consequent);
        }
    }

    fn declaration(&mut self, declaration: &Declaration<'_>) {
        match declaration {
            Declaration::VariableDeclaration(declaration) => {
                self.variable_declaration(declaration);
            }
            Declaration::FunctionDeclaration(function) => {
                if !function.declare {
                    self.function(function);
                }
            }
            // Erased by the upstream TypeScript transpile step.
            Declaration::TSTypeAliasDeclaration(_) | Declaration::TSInterfaceDeclaration(_) => {}
            _ => self.unsupported(declaration_kind(declaration), declaration.span()),
        }
    }

    fn module(&mut self, module: &ModuleDeclaration<'_>) {
        match module {
            // Erased by the upstream TypeScript transpile step.
            ModuleDeclaration::ImportDeclaration(import)
                if import.import_kind == ImportOrExportKind::Type => {}
            _ => self.unsupported(module_kind(module), module.span()),
        }
    }

    fn variable_declaration(&mut self, declaration: &VariableDeclaration<'_>) {
        if declaration.declare {
            return;
        }
        for declarator in &declaration.declarations {
            self.pattern(&declarator.id);
            if let Some(init) = &declarator.init {
                self.expression(init);
            }
        }
    }

    fn for_init(&mut self, init: &ForStatementInit<'_>) {
        match init {
            ForStatementInit::VariableDeclaration(declaration) => {
                self.variable_declaration(declaration);
            }
            _ => match init.as_expression() {
                Some(expression) => self.expression(expression),
                None => self.unsupported("ForStatement", init.span()),
            },
        }
    }

    fn for_left(&mut self, left: &ForStatementLeft<'_>) {
        match left {
            ForStatementLeft::VariableDeclaration(declaration) => {
                self.variable_declaration(declaration);
            }
            _ => match left.as_assignment_target() {
                Some(target) => self.assignment_target(target),
                None => self.unsupported("ForStatement", left.span()),
            },
        }
    }

    fn function(&mut self, function: &Function<'_>) {
        if function.generator {
            self.unsupported("FunctionDeclaration", function.span);
            return;
        }
        self.parameters(&function.params, function.span);
        if let Some(body) = &function.body {
            self.function_body(body);
        }
    }

    fn function_body(&mut self, body: &FunctionBody<'_>) {
        self.statements(&body.statements);
    }

    fn parameters(&mut self, parameters: &oxc_ast::ast::FormalParameters<'_>, span: Span) {
        for parameter in &parameters.items {
            if !parameter.decorators.is_empty() {
                self.unsupported("Decorator", parameter.span);
                continue;
            }
            self.pattern(&parameter.pattern);
            if let Some(initializer) = &parameter.initializer {
                self.expression(initializer);
            }
        }
        if let Some(rest) = &parameters.rest {
            if !rest.decorators.is_empty() {
                self.unsupported("Decorator", span);
            }
            self.pattern(&rest.rest.argument);
        }
    }

    fn arrow_function(&mut self, arrow: &ArrowFunctionExpression<'_>) {
        self.parameters(&arrow.params, arrow.span);
        match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => self.function_body(body),
            _ => match arrow.body.as_expression() {
                Some(expression) => self.expression(expression),
                None => self.unsupported("ArrowFunctionExpression", arrow.span),
            },
        }
    }

    fn expression(&mut self, expression: &Expression<'_>) {
        match expression {
            Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::RegExpLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::Identifier(_) => {}
            Expression::TemplateLiteral(literal) => {
                for expression in &literal.expressions {
                    self.expression(expression);
                }
            }
            Expression::ArrayExpression(array) => {
                for element in &array.elements {
                    match element {
                        ArrayExpressionElement::SpreadElement(spread) => {
                            self.expression(&spread.argument);
                        }
                        ArrayExpressionElement::Elision(_) => {}
                        _ => match element.as_expression() {
                            Some(expression) => self.expression(expression),
                            None => self.unsupported("ArrayExpression", element.span()),
                        },
                    }
                }
            }
            Expression::ArrowFunctionExpression(arrow) => self.arrow_function(arrow),
            Expression::AssignmentExpression(assignment) => {
                self.assignment_target(&assignment.left);
                self.expression(&assignment.right);
            }
            Expression::AwaitExpression(expression) => self.expression(&expression.argument),
            Expression::BinaryExpression(expression) => {
                self.expression(&expression.left);
                self.expression(&expression.right);
            }
            Expression::CallExpression(call) => {
                self.expression(&call.callee);
                self.arguments(&call.arguments);
            }
            Expression::ChainExpression(chain) => match &chain.expression {
                oxc_ast::ast::ChainElement::CallExpression(call) => {
                    self.expression(&call.callee);
                    self.arguments(&call.arguments);
                }
                oxc_ast::ast::ChainElement::TSNonNullExpression(expression) => {
                    self.expression(&expression.expression);
                }
                _ => match chain.expression.as_member_expression() {
                    Some(member) => self.member(member),
                    None => self.unsupported("ChainExpression", chain.span),
                },
            },
            Expression::ConditionalExpression(expression) => {
                self.expression(&expression.test);
                self.expression(&expression.consequent);
                self.expression(&expression.alternate);
            }
            Expression::FunctionExpression(function) => self.function(function),
            Expression::LogicalExpression(expression) => {
                self.expression(&expression.left);
                self.expression(&expression.right);
            }
            Expression::NewExpression(expression) => {
                let callee = unwrap_typescript(&expression.callee);
                if !matches!(callee, Expression::Identifier(_)) {
                    self.unsupported("NewExpression", expression.span);
                }
                self.arguments(&expression.arguments);
            }
            Expression::ObjectExpression(object) => {
                for property in &object.properties {
                    match property {
                        ObjectPropertyKind::ObjectProperty(property) => {
                            if property.kind != oxc_ast::ast::PropertyKind::Init {
                                self.unsupported("ObjectExpression", property.span);
                                continue;
                            }
                            if property.computed {
                                self.property_key(&property.key);
                            }
                            self.expression(&property.value);
                        }
                        ObjectPropertyKind::SpreadProperty(spread) => {
                            self.expression(&spread.argument);
                        }
                    }
                }
            }
            Expression::UnaryExpression(expression) => self.expression(&expression.argument),
            Expression::UpdateExpression(expression) => {
                self.simple_assignment_target(&expression.argument);
            }
            Expression::ParenthesizedExpression(expression) => {
                self.expression(&expression.expression)
            }
            Expression::TSAsExpression(expression) => self.expression(&expression.expression),
            Expression::TSNonNullExpression(expression) => self.expression(&expression.expression),
            Expression::TSSatisfiesExpression(expression) => {
                self.expression(&expression.expression)
            }
            Expression::TSTypeAssertion(expression) => self.expression(&expression.expression),
            Expression::TSInstantiationExpression(expression) => {
                self.expression(&expression.expression);
            }
            _ => match expression.as_member_expression() {
                Some(member) => self.member(member),
                None => self.unsupported(expression_kind(expression), expression.span()),
            },
        }
    }

    fn arguments(&mut self, arguments: &[oxc_ast::ast::Argument<'_>]) {
        for argument in arguments {
            match argument {
                oxc_ast::ast::Argument::SpreadElement(spread) => {
                    self.expression(&spread.argument);
                }
                _ => match argument.as_expression() {
                    Some(expression) => self.expression(expression),
                    None => self.unsupported("CallExpression", argument.span()),
                },
            }
        }
    }

    fn member(&mut self, member: &oxc_ast::ast::MemberExpression<'_>) {
        match member {
            oxc_ast::ast::MemberExpression::StaticMemberExpression(expression) => {
                self.expression(&expression.object);
            }
            oxc_ast::ast::MemberExpression::ComputedMemberExpression(expression) => {
                self.expression(&expression.object);
                self.expression(&expression.expression);
            }
            oxc_ast::ast::MemberExpression::PrivateFieldExpression(expression) => {
                self.unsupported("PrivateFieldExpression", expression.span);
            }
        }
    }

    fn property_key(&mut self, key: &PropertyKey<'_>) {
        match key {
            PropertyKey::StaticIdentifier(_) | PropertyKey::PrivateIdentifier(_) => {}
            _ => match key.as_expression() {
                Some(expression) => self.expression(expression),
                None => self.unsupported("ObjectExpression", key.span()),
            },
        }
    }

    fn assignment_target(&mut self, target: &AssignmentTarget<'_>) {
        if let Some(simple) = target.as_simple_assignment_target() {
            self.simple_assignment_target(simple);
            return;
        }
        match target.as_assignment_target_pattern() {
            Some(AssignmentTargetPattern::ArrayAssignmentTarget(pattern)) => {
                self.unsupported("ArrayPattern", pattern.span);
            }
            Some(AssignmentTargetPattern::ObjectAssignmentTarget(pattern)) => {
                self.unsupported("ObjectPattern", pattern.span);
            }
            None => self.unsupported("AssignmentExpression", target.span()),
        }
    }

    fn simple_assignment_target(&mut self, target: &SimpleAssignmentTarget<'_>) {
        match target {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(_) => {}
            SimpleAssignmentTarget::TSAsExpression(expression) => {
                self.expression(&expression.expression);
            }
            SimpleAssignmentTarget::TSNonNullExpression(expression) => {
                self.expression(&expression.expression);
            }
            SimpleAssignmentTarget::TSSatisfiesExpression(expression) => {
                self.expression(&expression.expression);
            }
            SimpleAssignmentTarget::TSTypeAssertion(expression) => {
                self.expression(&expression.expression);
            }
            _ => match target.as_member_expression() {
                Some(member) => self.member(member),
                None => self.unsupported("AssignmentExpression", target.span()),
            },
        }
    }

    fn pattern(&mut self, pattern: &BindingPattern<'_>) {
        match pattern {
            BindingPattern::BindingIdentifier(_) => {}
            BindingPattern::ObjectPattern(object) => {
                for property in &object.properties {
                    if property.computed {
                        self.property_key(&property.key);
                    }
                    self.pattern(&property.value);
                }
                if let Some(rest) = &object.rest {
                    self.pattern(&rest.argument);
                }
            }
            BindingPattern::ArrayPattern(array) => {
                for element in array.elements.iter().flatten() {
                    self.pattern(element);
                }
                if let Some(rest) = &array.rest {
                    self.pattern(&rest.argument);
                }
            }
            BindingPattern::AssignmentPattern(assignment) => {
                self.pattern(&assignment.left);
                self.expression(&assignment.right);
            }
        }
    }
}

fn unwrap_typescript<'a, 'b>(expression: &'b Expression<'a>) -> &'b Expression<'a> {
    match expression {
        Expression::ParenthesizedExpression(inner) => unwrap_typescript(&inner.expression),
        Expression::TSAsExpression(inner) => unwrap_typescript(&inner.expression),
        Expression::TSNonNullExpression(inner) => unwrap_typescript(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => unwrap_typescript(&inner.expression),
        Expression::TSTypeAssertion(inner) => unwrap_typescript(&inner.expression),
        Expression::TSInstantiationExpression(inner) => unwrap_typescript(&inner.expression),
        _ => expression,
    }
}

fn statement_kind(statement: &Statement<'_>) -> &'static str {
    match statement {
        Statement::LabeledStatement(_) => "LabeledStatement",
        Statement::DebuggerStatement(_) => "DebuggerStatement",
        Statement::WithStatement(_) => "WithStatement",
        _ => "Statement",
    }
}

fn declaration_kind(declaration: &Declaration<'_>) -> &'static str {
    match declaration {
        Declaration::ClassDeclaration(_) => "ClassDeclaration",
        Declaration::TSEnumDeclaration(_) => "TSEnumDeclaration",
        Declaration::TSExternalModuleDeclaration(_) => "TSExternalModuleDeclaration",
        Declaration::TSNamespaceDeclaration(_) => "TSNamespaceDeclaration",
        Declaration::TSGlobalDeclaration(_) => "TSGlobalDeclaration",
        Declaration::TSImportEqualsDeclaration(_) => "TSImportEqualsDeclaration",
        Declaration::VariableDeclaration(_)
        | Declaration::FunctionDeclaration(_)
        | Declaration::TSTypeAliasDeclaration(_)
        | Declaration::TSInterfaceDeclaration(_) => "Declaration",
    }
}

fn module_kind(module: &ModuleDeclaration<'_>) -> &'static str {
    match module {
        ModuleDeclaration::ImportDeclaration(_) => "ImportDeclaration",
        ModuleDeclaration::ExportAllDeclaration(_) => "ExportAllDeclaration",
        ModuleDeclaration::ExportDefaultDeclaration(_) => "ExportDefaultDeclaration",
        ModuleDeclaration::ExportDeclaration(_) => "ExportDeclaration",
        ModuleDeclaration::ExportNamedDeclaration(_) => "ExportNamedDeclaration",
        ModuleDeclaration::ExportFromDeclaration(_) => "ExportFromDeclaration",
        ModuleDeclaration::TSExportAssignment(_) => "TSExportAssignment",
        ModuleDeclaration::TSNamespaceExportDeclaration(_) => "TSNamespaceExportDeclaration",
    }
}

fn expression_kind(expression: &Expression<'_>) -> &'static str {
    match expression {
        Expression::Super(_) => "Super",
        Expression::ClassExpression(_) => "ClassExpression",
        Expression::ImportExpression(_) => "ImportExpression",
        Expression::SequenceExpression(_) => "SequenceExpression",
        Expression::TaggedTemplateExpression(_) => "TaggedTemplateExpression",
        Expression::ThisExpression(_) => "ThisExpression",
        Expression::YieldExpression(_) => "YieldExpression",
        Expression::PrivateInExpression(_) => "PrivateInExpression",
        Expression::ImportMeta(_) => "ImportMeta",
        Expression::NewTarget(_) => "NewTarget",
        Expression::JSXElement(_) => "JSXElement",
        Expression::JSXFragment(_) => "JSXFragment",
        Expression::V8IntrinsicExpression(_) => "V8IntrinsicExpression",
        _ => "Expression",
    }
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;

    use super::{inspect_support, UnsupportedSyntax};
    use crate::parse_program;

    fn unsupported_kinds(source: &str) -> Vec<&'static str> {
        let allocator = Allocator::default();
        let program = parse_program(&allocator, source).expect("fixture should parse");
        inspect_support(&program)
            .unsupported()
            .iter()
            .map(UnsupportedSyntax::kind)
            .collect()
    }

    #[test]
    fn accepts_upstream_orchestration_subset() {
        let source = r#"
            interface Row { id: string }
            type RowId = string
            import type { Unused } from "./unused";
            declare function injected(): void;

            const query: string = `rust ${"parser"}`;
            const { data: rows = [] } = await tools.search({ query });
            const ids = rows?.map((row: Row) => row.id as RowId) ?? [];
            for (const id of ids) {
                console.log(id!, ...ids);
            }
            try {
                return await tools.fetch({ id: ids[0] });
            } catch (error) {
                throw error;
            } finally {
                console.log(Object.keys(tools));
            }
        "#;

        assert_eq!(unsupported_kinds(source), Vec::<&'static str>::new());
    }

    #[test]
    fn reports_unsupported_statement_and_expression_kinds() {
        let source = r#"
            class Declared {}
            const anonymous = class {};
            const self = this;
            const tagged = tag`value`;
            const sequence = (1, 2);
            function* generator() {}
        "#;

        assert_eq!(
            unsupported_kinds(source),
            vec![
                "ClassDeclaration",
                "ClassExpression",
                "ThisExpression",
                "TaggedTemplateExpression",
                "SequenceExpression",
                "FunctionDeclaration",
            ]
        );
    }

    #[test]
    fn reports_labels_and_destructuring_assignment() {
        let source = r#"
            let value = 0;
            [value] = [1];
            outer: for (;;) { break outer; }
        "#;

        assert_eq!(
            unsupported_kinds(source),
            vec!["ArrayPattern", "LabeledStatement"]
        );
    }

    #[test]
    fn restricts_new_expressions_to_identifier_callees() {
        assert!(unsupported_kinds("const date = new Date();").is_empty());
        assert_eq!(
            unsupported_kinds("const made = new tools.Factory();"),
            vec!["NewExpression"]
        );
    }

    #[test]
    fn keeps_lazy_typescript_syntax_transparent() {
        let source = r#"
            const identity = <T,>(value: T): T => value;
            const result = identity<string>("value")!;
            const object = { ["computed" + "Key"]: result } satisfies Record<string, string>;
        "#;

        assert!(unsupported_kinds(source).is_empty());
    }
}
