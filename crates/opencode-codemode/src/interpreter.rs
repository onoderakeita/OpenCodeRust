//! A synchronous evaluator for the upstream CodeMode syntax subset.
//!
//! This is the first execution baseline: it implements variables, control flow,
//! functions and closures, destructuring, data literals, operators, member
//! access, optional chaining, exceptions, and top-level `await` as a
//! pass-through. Tool calls, promises, and the standard library are separate
//! stages and are not implemented here.

use std::cell::RefCell;
use std::fmt::{self, Display, Formatter};
use std::rc::Rc;

use indexmap::IndexMap;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, ArrowFunctionBody, ArrowFunctionExpression, AssignmentTarget,
    BinaryExpression, BindingPattern, ChainElement, Expression, ForStatementInit, ForStatementLeft,
    Function, FunctionBody, LogicalOperator, MemberExpression, ObjectPropertyKind, Program,
    PropertyKey, SimpleAssignmentTarget, Statement, UnaryOperator, UpdateOperator,
    VariableDeclaration, VariableDeclarationKind,
};
use oxc_span::{GetSpan, Span};
use oxc_syntax::operator::BinaryOperator;

use crate::value::{is_blocked_member, FunctionId, Value};

/// Classification of an interpreter runtime error, mirroring the upstream
/// diagnostic kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeErrorKind {
    /// Unknown identifier or assignment to an undeclared binding.
    ReferenceError,
    /// Invalid operand or target type.
    TypeError,
    /// Invalid numeric or structural operation.
    RangeError,
    /// A host value that is not plain data.
    InvalidDataValue,
    /// Syntax accepted by the parser but outside the interpreter subset.
    UnsupportedSyntax,
    /// Any other execution failure.
    ExecutionFailure,
}

impl Display for RuntimeErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReferenceError => "ReferenceError",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::InvalidDataValue => "InvalidDataValue",
            Self::UnsupportedSyntax => "UnsupportedSyntax",
            Self::ExecutionFailure => "ExecutionFailure",
        })
    }
}

/// An error raised while evaluating a program.
#[derive(Debug, Clone)]
pub struct RuntimeError {
    kind: RuntimeErrorKind,
    message: String,
    span: Span,
    thrown: Option<Value>,
}

impl RuntimeError {
    fn new(kind: RuntimeErrorKind, message: impl Into<String>, span: Span) -> Self {
        Self {
            kind,
            message: message.into(),
            span,
            thrown: None,
        }
    }

    fn reference(message: impl Into<String>, span: Span) -> Self {
        Self::new(RuntimeErrorKind::ReferenceError, message, span)
    }

    fn type_error(message: impl Into<String>, span: Span) -> Self {
        Self::new(RuntimeErrorKind::TypeError, message, span)
    }

    fn unsupported(syntax: &str, span: Span) -> Self {
        Self::new(
            RuntimeErrorKind::UnsupportedSyntax,
            format!("Syntax '{syntax}' is not supported in CodeMode."),
            span,
        )
    }

    fn thrown(value: Value, span: Span) -> Self {
        Self {
            kind: RuntimeErrorKind::ExecutionFailure,
            message: to_js_string(&value),
            span,
            thrown: Some(value),
        }
    }

    /// Error kind.
    pub fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    /// Human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Source span of the failing node.
    pub fn span(&self) -> Span {
        self.span
    }

    /// The value passed to `throw`, when the failure originated from one.
    pub fn thrown_value(&self) -> Option<&Value> {
        self.thrown.as_ref()
    }

    fn caught_value(&self) -> Value {
        self.thrown.clone().unwrap_or_else(|| {
            Value::from_entries([("message", Value::String(self.message.clone()))])
        })
    }
}

impl Display for RuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug, Clone)]
struct Binding {
    value: Value,
    mutable: bool,
}

#[derive(Debug, Default)]
struct Scope {
    parent: Option<Rc<RefCell<Scope>>>,
    bindings: IndexMap<String, Binding>,
    function_scope: bool,
}

impl Scope {
    fn child(parent: Rc<RefCell<Scope>>) -> Self {
        Self {
            parent: Some(parent),
            bindings: IndexMap::new(),
            function_scope: false,
        }
    }

    fn global() -> Self {
        Self {
            parent: None,
            bindings: IndexMap::new(),
            function_scope: true,
        }
    }

    fn function(parent: Option<Rc<RefCell<Scope>>>) -> Self {
        Self {
            parent,
            bindings: IndexMap::new(),
            function_scope: true,
        }
    }
}

/// Completion produced by evaluating a statement.
#[derive(Debug, Clone)]
enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

#[derive(Debug, Clone, Copy)]
enum FunctionBodyDef<'a> {
    Block(&'a FunctionBody<'a>),
    Expression(&'a Expression<'a>),
}

#[derive(Clone)]
struct FunctionDef<'a> {
    params: &'a oxc_ast::ast::FormalParameters<'a>,
    body: FunctionBodyDef<'a>,
    closure: Rc<RefCell<Scope>>,
}

/// Interpreter over one or more parsed programs.
pub struct Interpreter<'a> {
    functions: Vec<FunctionDef<'a>>,
    scope: Rc<RefCell<Scope>>,
}

impl<'a> Default for Interpreter<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Interpreter<'a> {
    /// Creates an interpreter with a fresh global scope.
    pub fn new() -> Self {
        Self {
            functions: Vec::new(),
            scope: Rc::new(RefCell::new(Scope::global())),
        }
    }

    /// Evaluates a program and returns its top-level `return` value, or
    /// `undefined`.
    pub fn run(&mut self, program: &'a Program<'a>) -> Result<Value, RuntimeError> {
        self.hoist_functions(&program.body)?;
        for statement in &program.body {
            match self.eval_statement(statement)? {
                Flow::Normal => {}
                Flow::Return(value) => return Ok(value),
                Flow::Break | Flow::Continue => {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::ExecutionFailure,
                        "Illegal break or continue statement.",
                        statement.span(),
                    ));
                }
            }
        }
        Ok(Value::Undefined)
    }

    fn hoist_functions(&mut self, statements: &'a [Statement<'a>]) -> Result<(), RuntimeError> {
        for statement in statements {
            if let Statement::FunctionDeclaration(function) = statement {
                if function.declare {
                    continue;
                }
                let id = self.create_function(function)?;
                let name = function
                    .id
                    .as_ref()
                    .map_or_else(String::new, |id| id.name.as_str().to_owned());
                self.declare(&name, Value::Function(id), true, false, function.span)?;
            }
        }
        Ok(())
    }

    fn create_function(&mut self, function: &'a Function<'a>) -> Result<FunctionId, RuntimeError> {
        if function.generator {
            return Err(RuntimeError::unsupported(
                "FunctionDeclaration",
                function.span,
            ));
        }
        let body = function
            .body
            .as_deref()
            .ok_or_else(|| RuntimeError::unsupported("FunctionDeclaration", function.span))?;
        let id = FunctionId(self.functions.len());
        self.functions.push(FunctionDef {
            params: &function.params,
            body: FunctionBodyDef::Block(body),
            closure: Rc::clone(&self.scope),
        });
        Ok(id)
    }

    fn create_arrow(&mut self, arrow: &'a ArrowFunctionExpression<'a>) -> FunctionId {
        let body = match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => FunctionBodyDef::Block(body),
            _ => FunctionBodyDef::Expression(
                arrow
                    .body
                    .as_expression()
                    .expect("arrow body is either a block or an expression"),
            ),
        };
        let id = FunctionId(self.functions.len());
        self.functions.push(FunctionDef {
            params: &arrow.params,
            body,
            closure: Rc::clone(&self.scope),
        });
        id
    }

    fn eval_statement(&mut self, statement: &'a Statement<'a>) -> Result<Flow, RuntimeError> {
        if let Some(declaration) = statement.as_declaration() {
            match declaration {
                oxc_ast::ast::Declaration::VariableDeclaration(declaration) => {
                    self.eval_variable_declaration(declaration)?;
                    return Ok(Flow::Normal);
                }
                oxc_ast::ast::Declaration::FunctionDeclaration(_) => {
                    return Ok(Flow::Normal);
                }
                oxc_ast::ast::Declaration::TSTypeAliasDeclaration(_)
                | oxc_ast::ast::Declaration::TSInterfaceDeclaration(_) => {
                    return Ok(Flow::Normal);
                }
                _ => {
                    return Err(RuntimeError::unsupported("Declaration", declaration.span()));
                }
            }
        }

        match statement {
            Statement::BlockStatement(block) => self.eval_block(&block.body),
            Statement::BreakStatement(_) => Ok(Flow::Break),
            Statement::ContinueStatement(_) => Ok(Flow::Continue),
            Statement::DoWhileStatement(statement) => loop {
                match self.eval_statement(&statement.body)? {
                    Flow::Normal | Flow::Continue => {}
                    Flow::Break => return Ok(Flow::Normal),
                    Flow::Return(value) => return Ok(Flow::Return(value)),
                }
                let test = self.eval_expression(&statement.test)?;
                if !is_truthy(&test) {
                    return Ok(Flow::Normal);
                }
            },
            Statement::EmptyStatement(_) => Ok(Flow::Normal),
            Statement::ExpressionStatement(statement) => {
                self.eval_expression(&statement.expression)?;
                Ok(Flow::Normal)
            }
            Statement::ForInStatement(statement) => {
                let value = self.eval_expression(&statement.right)?;
                let keys = enumerable_keys(&value);
                self.eval_for_each(&statement.left, keys, &statement.body)
            }
            Statement::ForOfStatement(statement) => {
                let value = self.eval_expression(&statement.right)?;
                let items = iterable_items(&value, statement.span)?;
                self.eval_for_each(&statement.left, items, &statement.body)
            }
            Statement::ForStatement(statement) => self.eval_for_statement(statement),
            Statement::IfStatement(statement) => {
                let test = self.eval_expression(&statement.test)?;
                if is_truthy(&test) {
                    self.eval_statement(&statement.consequent)
                } else if let Some(alternate) = &statement.alternate {
                    self.eval_statement(alternate)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Statement::ReturnStatement(statement) => {
                let value = match &statement.argument {
                    Some(argument) => self.eval_expression(argument)?,
                    None => Value::Undefined,
                };
                Ok(Flow::Return(value))
            }
            Statement::SwitchStatement(statement) => self.eval_switch(statement),
            Statement::ThrowStatement(statement) => {
                let value = self.eval_expression(&statement.argument)?;
                Err(RuntimeError::thrown(value, statement.span))
            }
            Statement::TryStatement(statement) => {
                let attempted = self.eval_block(&statement.block.body);
                let result = match attempted {
                    Ok(flow) => Ok(flow),
                    Err(error) => match &statement.handler {
                        Some(handler) => self.eval_catch(handler, &error),
                        None => Err(error),
                    },
                };
                match &statement.finalizer {
                    Some(finalizer) => match self.eval_block(&finalizer.body)? {
                        Flow::Normal => result,
                        abrupt => Ok(abrupt),
                    },
                    None => result,
                }
            }
            Statement::WhileStatement(statement) => loop {
                let test = self.eval_expression(&statement.test)?;
                if !is_truthy(&test) {
                    return Ok(Flow::Normal);
                }
                match self.eval_statement(&statement.body)? {
                    Flow::Normal | Flow::Continue => {}
                    Flow::Break => return Ok(Flow::Normal),
                    Flow::Return(value) => return Ok(Flow::Return(value)),
                }
            },
            _ => Err(RuntimeError::unsupported(
                statement_kind(statement),
                statement.span(),
            )),
        }
    }

    fn eval_catch(
        &mut self,
        handler: &'a oxc_ast::ast::CatchClause<'a>,
        error: &RuntimeError,
    ) -> Result<Flow, RuntimeError> {
        let saved = Rc::clone(&self.scope);
        self.scope = Rc::new(RefCell::new(Scope::child(saved.clone())));
        let result = (|| {
            if let Some(parameter) = &handler.param {
                self.bind_pattern(&parameter.pattern, error.caught_value(), true, false)?;
            }
            self.eval_block(&handler.body.body)
        })();
        self.scope = saved;
        result
    }

    fn eval_block(&mut self, statements: &'a [Statement<'a>]) -> Result<Flow, RuntimeError> {
        let saved = Rc::clone(&self.scope);
        self.scope = Rc::new(RefCell::new(Scope::child(saved.clone())));
        let result = (|| {
            self.hoist_functions(statements)?;
            for statement in statements {
                match self.eval_statement(statement)? {
                    Flow::Normal => {}
                    abrupt => return Ok(abrupt),
                }
            }
            Ok(Flow::Normal)
        })();
        self.scope = saved;
        result
    }

    fn eval_variable_declaration(
        &mut self,
        declaration: &'a VariableDeclaration<'a>,
    ) -> Result<(), RuntimeError> {
        let function_scoped = declaration.kind == VariableDeclarationKind::Var;
        let mutable = declaration.kind != VariableDeclarationKind::Const;
        for declarator in &declaration.declarations {
            let value = match &declarator.init {
                Some(init) => self.eval_expression(init)?,
                None => Value::Undefined,
            };
            self.bind_pattern(&declarator.id, value, mutable, function_scoped)?;
        }
        Ok(())
    }

    fn eval_for_statement(
        &mut self,
        statement: &'a oxc_ast::ast::ForStatement<'a>,
    ) -> Result<Flow, RuntimeError> {
        let saved = Rc::clone(&self.scope);
        self.scope = Rc::new(RefCell::new(Scope::child(saved.clone())));
        let result = (|| {
            if let Some(init) = &statement.init {
                match init {
                    ForStatementInit::VariableDeclaration(declaration) => {
                        self.eval_variable_declaration(declaration)?;
                    }
                    _ => {
                        if let Some(expression) = init.as_expression() {
                            self.eval_expression(expression)?;
                        }
                    }
                }
            }
            loop {
                if let Some(test) = &statement.test {
                    let value = self.eval_expression(test)?;
                    if !is_truthy(&value) {
                        return Ok(Flow::Normal);
                    }
                }
                match self.eval_statement(&statement.body)? {
                    Flow::Normal | Flow::Continue => {}
                    Flow::Break => return Ok(Flow::Normal),
                    Flow::Return(value) => return Ok(Flow::Return(value)),
                }
                if let Some(update) = &statement.update {
                    self.eval_expression(update)?;
                }
            }
        })();
        self.scope = saved;
        result
    }

    fn eval_for_each(
        &mut self,
        left: &'a ForStatementLeft<'a>,
        items: Vec<Value>,
        body: &'a Statement<'a>,
    ) -> Result<Flow, RuntimeError> {
        for item in items {
            let saved = Rc::clone(&self.scope);
            self.scope = Rc::new(RefCell::new(Scope::child(saved.clone())));
            let iteration = (|| {
                match left {
                    ForStatementLeft::VariableDeclaration(declaration) => {
                        let declarator = declaration.declarations.first().ok_or_else(|| {
                            RuntimeError::type_error("Invalid for-loop binding.", declaration.span)
                        })?;
                        self.bind_pattern(
                            &declarator.id,
                            item,
                            declaration.kind != VariableDeclarationKind::Const,
                            false,
                        )?;
                    }
                    _ => {
                        let target = left.as_assignment_target().ok_or_else(|| {
                            RuntimeError::type_error("Invalid for-loop target.", left.span())
                        })?;
                        self.assign_target(target, item)?;
                    }
                }
                self.eval_statement(body)
            })();
            self.scope = saved;
            match iteration? {
                Flow::Normal | Flow::Continue => {}
                Flow::Break => return Ok(Flow::Normal),
                Flow::Return(value) => return Ok(Flow::Return(value)),
            }
        }
        Ok(Flow::Normal)
    }

    fn eval_switch(
        &mut self,
        statement: &'a oxc_ast::ast::SwitchStatement<'a>,
    ) -> Result<Flow, RuntimeError> {
        let discriminant = self.eval_expression(&statement.discriminant)?;
        let mut matched = None;
        let mut default = None;
        for (index, case) in statement.cases.iter().enumerate() {
            match &case.test {
                Some(test) => {
                    if matched.is_none() {
                        let value = self.eval_expression(test)?;
                        if strict_equals(&discriminant, &value) {
                            matched = Some(index);
                            break;
                        }
                    }
                }
                None => default = Some(index),
            }
        }

        let start = matched.or(default);
        if let Some(start) = start {
            for case in &statement.cases[start..] {
                for statement in &case.consequent {
                    match self.eval_statement(statement)? {
                        Flow::Normal => {}
                        Flow::Break => return Ok(Flow::Normal),
                        abrupt => return Ok(abrupt),
                    }
                }
            }
        }
        Ok(Flow::Normal)
    }

    fn eval_expression(&mut self, expression: &'a Expression<'a>) -> Result<Value, RuntimeError> {
        match expression {
            Expression::BooleanLiteral(literal) => Ok(Value::Boolean(literal.value)),
            Expression::NullLiteral(_) => Ok(Value::Null),
            Expression::NumericLiteral(literal) => Ok(Value::Number(literal.value)),
            Expression::StringLiteral(literal) => {
                Ok(Value::String(literal.value.as_str().to_owned()))
            }
            Expression::BigIntLiteral(_) => Err(RuntimeError::unsupported(
                "BigIntLiteral",
                expression.span(),
            )),
            Expression::RegExpLiteral(_) => Err(RuntimeError::unsupported(
                "RegExpLiteral",
                expression.span(),
            )),
            Expression::TemplateLiteral(literal) => {
                let mut output = String::new();
                for (index, quasi) in literal.quasis.iter().enumerate() {
                    if let Some(cooked) = &quasi.value.cooked {
                        output.push_str(cooked.as_str());
                    }
                    if let Some(value) = literal.expressions.get(index) {
                        let value = self.eval_expression(value)?;
                        output.push_str(&to_js_string(&value));
                    }
                }
                Ok(Value::String(output))
            }
            Expression::Identifier(identifier) => {
                self.lookup(identifier.name.as_str()).ok_or_else(|| {
                    RuntimeError::reference(
                        format!("Unknown identifier '{}'.", identifier.name),
                        identifier.span,
                    )
                })
            }
            Expression::ArrayExpression(array) => {
                let mut values = Vec::new();
                for element in &array.elements {
                    match element {
                        ArrayExpressionElement::Elision(_) => values.push(Value::Undefined),
                        ArrayExpressionElement::SpreadElement(spread) => {
                            let value = self.eval_expression(&spread.argument)?;
                            values.extend(spread_items(&value).ok_or_else(|| {
                                RuntimeError::type_error(
                                    "Spread requires an array or string in CodeMode.",
                                    spread.span,
                                )
                            })?);
                        }
                        _ => {
                            let value = element
                                .as_expression()
                                .ok_or_else(|| {
                                    RuntimeError::unsupported("ArrayExpression", element.span())
                                })
                                .and_then(|expression| self.eval_expression(expression))?;
                            values.push(value);
                        }
                    }
                }
                Ok(Value::array(values))
            }
            Expression::ObjectExpression(object) => {
                let mut values = IndexMap::new();
                for property in &object.properties {
                    match property {
                        ObjectPropertyKind::SpreadProperty(spread) => {
                            let value = self.eval_expression(&spread.argument)?;
                            match value {
                                Value::Null | Value::Undefined => {}
                                Value::Object(source) => {
                                    for (key, value) in source.borrow().iter() {
                                        if is_blocked_member(key) {
                                            return Err(RuntimeError::type_error(
                                                format!("Property '{key}' is not available in CodeMode."),
                                                spread.span,
                                            ));
                                        }
                                        values.insert(key.clone(), value.clone());
                                    }
                                }
                                _ => {
                                    return Err(RuntimeError::type_error(
                                        "Object spread requires a data object in CodeMode.",
                                        spread.span,
                                    ));
                                }
                            }
                        }
                        ObjectPropertyKind::ObjectProperty(property) => {
                            if property.kind != oxc_ast::ast::PropertyKind::Init {
                                return Err(RuntimeError::unsupported(
                                    "ObjectExpression",
                                    property.span,
                                ));
                            }
                            let key = if property.computed {
                                let key =
                                    self.eval_expression(property_key_expression(&property.key)?)?;
                                to_property_key(&key, property.key.span())?
                            } else {
                                static_property_key(&property.key)?
                            };
                            if is_blocked_member(&key) {
                                return Err(RuntimeError::type_error(
                                    format!("Property '{key}' is not available in CodeMode."),
                                    property.key.span(),
                                ));
                            }
                            let value = self.eval_expression(&property.value)?;
                            values.insert(key, value);
                        }
                    }
                }
                Ok(Value::Object(Rc::new(RefCell::new(values))))
            }
            Expression::ArrowFunctionExpression(arrow) => {
                let id = self.create_arrow(arrow);
                Ok(Value::Function(id))
            }
            Expression::FunctionExpression(function) => {
                let id = self.create_function(function)?;
                Ok(Value::Function(id))
            }
            Expression::CallExpression(call) => {
                let callee = self.eval_expression(&call.callee)?;
                if call.optional && callee.is_nullish() {
                    return Ok(Value::Undefined);
                }
                let arguments = self.eval_arguments(&call.arguments)?;
                self.invoke_value(&callee, arguments, call.span)
            }
            Expression::ChainExpression(chain) => Ok(self
                .eval_chain(&chain.expression)?
                .unwrap_or(Value::Undefined)),
            Expression::AwaitExpression(expression) => self.eval_expression(&expression.argument),
            Expression::UnaryExpression(expression) => {
                self.eval_unary(expression.operator, &expression.argument, expression.span)
            }
            Expression::BinaryExpression(expression) => self.eval_binary(expression),
            Expression::LogicalExpression(expression) => {
                let left = self.eval_expression(&expression.left)?;
                match expression.operator {
                    LogicalOperator::Or if is_truthy(&left) => Ok(left),
                    LogicalOperator::And if !is_truthy(&left) => Ok(left),
                    LogicalOperator::Coalesce if !left.is_nullish() => Ok(left),
                    _ => self.eval_expression(&expression.right),
                }
            }
            Expression::AssignmentExpression(expression) => self.eval_assignment(expression),
            Expression::UpdateExpression(expression) => self.eval_update(
                expression.operator,
                &expression.argument,
                expression.prefix,
                expression.span,
            ),
            Expression::ConditionalExpression(expression) => {
                let test = self.eval_expression(&expression.test)?;
                if is_truthy(&test) {
                    self.eval_expression(&expression.consequent)
                } else {
                    self.eval_expression(&expression.alternate)
                }
            }
            Expression::ParenthesizedExpression(expression) => {
                self.eval_expression(&expression.expression)
            }
            Expression::TSAsExpression(expression) => self.eval_expression(&expression.expression),
            Expression::TSNonNullExpression(expression) => {
                self.eval_expression(&expression.expression)
            }
            Expression::TSSatisfiesExpression(expression) => {
                self.eval_expression(&expression.expression)
            }
            Expression::TSTypeAssertion(expression) => self.eval_expression(&expression.expression),
            Expression::TSInstantiationExpression(expression) => {
                self.eval_expression(&expression.expression)
            }
            _ => match expression.as_member_expression() {
                Some(member) => self.eval_member(member),
                None => Err(RuntimeError::unsupported(
                    expression_kind(expression),
                    expression.span(),
                )),
            },
        }
    }

    fn eval_arguments(
        &mut self,
        arguments: &'a [Argument<'a>],
    ) -> Result<Vec<Value>, RuntimeError> {
        let mut values = Vec::new();
        for argument in arguments {
            match argument {
                Argument::SpreadElement(spread) => {
                    let value = self.eval_expression(&spread.argument)?;
                    values.extend(spread_items(&value).ok_or_else(|| {
                        RuntimeError::type_error(
                            "Spread arguments require an array or string in CodeMode.",
                            spread.span,
                        )
                    })?);
                }
                _ => {
                    let expression = argument.as_expression().ok_or_else(|| {
                        RuntimeError::unsupported("CallExpression", argument.span())
                    })?;
                    values.push(self.eval_expression(expression)?);
                }
            }
        }
        Ok(values)
    }

    fn eval_unary(
        &mut self,
        operator: UnaryOperator,
        argument: &'a Expression<'a>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if operator == UnaryOperator::Typeof {
            if let Expression::Identifier(identifier) = argument {
                if self.lookup(identifier.name.as_str()).is_none() {
                    return Ok(Value::String("undefined".to_owned()));
                }
            }
        }
        let value = self.eval_expression(argument)?;
        match operator {
            UnaryOperator::LogicalNot => Ok(Value::Boolean(!is_truthy(&value))),
            UnaryOperator::Typeof => Ok(Value::String(type_of(&value).to_owned())),
            UnaryOperator::UnaryPlus => Ok(Value::Number(to_number(&value))),
            UnaryOperator::UnaryNegation => Ok(Value::Number(-to_number(&value))),
            UnaryOperator::BitwiseNot => Ok(Value::Number(f64::from(!to_int32(to_number(&value))))),
            UnaryOperator::Void => Err(RuntimeError::unsupported("void", span)),
            UnaryOperator::Delete => Err(RuntimeError::unsupported("delete", span)),
        }
    }

    fn eval_binary(&mut self, expression: &'a BinaryExpression<'a>) -> Result<Value, RuntimeError> {
        let left = self.eval_expression(&expression.left)?;
        let right = self.eval_expression(&expression.right)?;
        self.apply_binary(expression.operator, &left, &right, expression.span)
    }

    fn apply_binary(
        &self,
        operator: BinaryOperator,
        left: &Value,
        right: &Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match operator {
            BinaryOperator::Equality => Ok(Value::Boolean(loose_equals(left, right))),
            BinaryOperator::Inequality => Ok(Value::Boolean(!loose_equals(left, right))),
            BinaryOperator::StrictEquality => Ok(Value::Boolean(strict_equals(left, right))),
            BinaryOperator::StrictInequality => Ok(Value::Boolean(!strict_equals(left, right))),
            BinaryOperator::LessThan => Ok(Value::Boolean(compare(left, right, span)? < 0.0)),
            BinaryOperator::LessEqualThan => Ok(Value::Boolean(compare(left, right, span)? <= 0.0)),
            BinaryOperator::GreaterThan => Ok(Value::Boolean(compare(left, right, span)? > 0.0)),
            BinaryOperator::GreaterEqualThan => {
                Ok(Value::Boolean(compare(left, right, span)? >= 0.0))
            }
            BinaryOperator::Addition => {
                if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
                    Ok(Value::String(format!(
                        "{}{}",
                        to_js_string(left),
                        to_js_string(right)
                    )))
                } else {
                    Ok(Value::Number(to_number(left) + to_number(right)))
                }
            }
            BinaryOperator::Subtraction => Ok(Value::Number(to_number(left) - to_number(right))),
            BinaryOperator::Multiplication => Ok(Value::Number(to_number(left) * to_number(right))),
            BinaryOperator::Division => Ok(Value::Number(to_number(left) / to_number(right))),
            BinaryOperator::Remainder => Ok(Value::Number(to_number(left) % to_number(right))),
            BinaryOperator::Exponential => {
                Ok(Value::Number(to_number(left).powf(to_number(right))))
            }
            BinaryOperator::ShiftLeft => Ok(Value::Number(f64::from(
                to_int32(to_number(left)) << (to_uint32(to_number(right)) & 31),
            ))),
            BinaryOperator::ShiftRight => Ok(Value::Number(f64::from(
                to_int32(to_number(left)) >> (to_uint32(to_number(right)) & 31),
            ))),
            BinaryOperator::ShiftRightZeroFill => Ok(Value::Number(f64::from(
                (to_int32(to_number(left)) as u32) >> (to_uint32(to_number(right)) & 31),
            ))),
            BinaryOperator::BitwiseOR => Ok(Value::Number(f64::from(
                to_int32(to_number(left)) | to_int32(to_number(right)),
            ))),
            BinaryOperator::BitwiseXOR => Ok(Value::Number(f64::from(
                to_int32(to_number(left)) ^ to_int32(to_number(right)),
            ))),
            BinaryOperator::BitwiseAnd => Ok(Value::Number(f64::from(
                to_int32(to_number(left)) & to_int32(to_number(right)),
            ))),
            BinaryOperator::In => {
                let key = to_property_key(left, span)?;
                Ok(Value::Boolean(has_property(right, &key)))
            }
            BinaryOperator::Instanceof => Err(RuntimeError::unsupported("instanceof", span)),
        }
    }

    fn eval_assignment(
        &mut self,
        expression: &'a oxc_ast::ast::AssignmentExpression<'a>,
    ) -> Result<Value, RuntimeError> {
        if expression.operator.is_assign() {
            let value = self.eval_expression(&expression.right)?;
            self.assign_target(&expression.left, value.clone())?;
            return Ok(value);
        }

        if let Some(logical) = expression.operator.to_logical_operator() {
            let current = self.read_target(&expression.left)?;
            let should_assign = match logical {
                LogicalOperator::Or => !is_truthy(&current),
                LogicalOperator::And => is_truthy(&current),
                LogicalOperator::Coalesce => current.is_nullish(),
            };
            if !should_assign {
                return Ok(current);
            }
            let value = self.eval_expression(&expression.right)?;
            self.assign_target(&expression.left, value.clone())?;
            return Ok(value);
        }

        let binary = expression
            .operator
            .to_binary_operator()
            .ok_or_else(|| RuntimeError::unsupported("AssignmentExpression", expression.span))?;
        let current = self.read_target(&expression.left)?;
        let right = self.eval_expression(&expression.right)?;
        let value = self.apply_binary(binary, &current, &right, expression.span)?;
        self.assign_target(&expression.left, value.clone())?;
        Ok(value)
    }

    fn eval_update(
        &mut self,
        operator: UpdateOperator,
        target: &'a SimpleAssignmentTarget<'a>,
        prefix: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let current = self.read_simple_target(target)?;
        let old = to_number(&current);
        let next = match operator {
            UpdateOperator::Increment => old + 1.0,
            UpdateOperator::Decrement => old - 1.0,
        };
        self.assign_simple_target(target, Value::Number(next))?;
        let _ = span;
        Ok(Value::Number(if prefix { next } else { old }))
    }

    fn eval_member(&mut self, member: &'a MemberExpression<'a>) -> Result<Value, RuntimeError> {
        match member {
            MemberExpression::StaticMemberExpression(member) => {
                let object = self.eval_expression(&member.object)?;
                if member.optional && object.is_nullish() {
                    return Ok(Value::Undefined);
                }
                self.get_property(&object, member.property.name.as_str(), member.span)
            }
            MemberExpression::ComputedMemberExpression(member) => {
                let object = self.eval_expression(&member.object)?;
                if member.optional && object.is_nullish() {
                    return Ok(Value::Undefined);
                }
                let key = self.eval_expression(&member.expression)?;
                let key = to_property_key(&key, member.span)?;
                self.get_property(&object, &key, member.span)
            }
            MemberExpression::PrivateFieldExpression(member) => Err(RuntimeError::unsupported(
                "PrivateFieldExpression",
                member.span,
            )),
        }
    }

    fn eval_chain(&mut self, element: &'a ChainElement<'a>) -> Result<Option<Value>, RuntimeError> {
        match element {
            ChainElement::CallExpression(call) => self.eval_chain_call(call),
            ChainElement::TSNonNullExpression(expression) => {
                self.eval_chain_value(&expression.expression)
            }
            _ => match element.as_member_expression() {
                Some(MemberExpression::StaticMemberExpression(member)) => {
                    self.eval_chain_static(member)
                }
                Some(MemberExpression::ComputedMemberExpression(member)) => {
                    self.eval_chain_computed(member)
                }
                Some(MemberExpression::PrivateFieldExpression(member)) => Err(
                    RuntimeError::unsupported("PrivateFieldExpression", member.span),
                ),
                None => Err(RuntimeError::unsupported("ChainExpression", element.span())),
            },
        }
    }

    fn eval_chain_value(
        &mut self,
        expression: &'a Expression<'a>,
    ) -> Result<Option<Value>, RuntimeError> {
        match expression {
            Expression::StaticMemberExpression(member) => self.eval_chain_static(member),
            Expression::ComputedMemberExpression(member) => self.eval_chain_computed(member),
            Expression::CallExpression(call) => self.eval_chain_call(call),
            Expression::TSNonNullExpression(expression) => {
                self.eval_chain_value(&expression.expression)
            }
            _ => Ok(Some(self.eval_expression(expression)?)),
        }
    }

    fn eval_chain_static(
        &mut self,
        member: &'a oxc_ast::ast::StaticMemberExpression<'a>,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(object) = self.eval_chain_value(&member.object)? else {
            return Ok(None);
        };
        if member.optional && object.is_nullish() {
            return Ok(None);
        }
        let value = self.get_property(&object, member.property.name.as_str(), member.span)?;
        Ok(Some(value))
    }

    fn eval_chain_computed(
        &mut self,
        member: &'a oxc_ast::ast::ComputedMemberExpression<'a>,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(object) = self.eval_chain_value(&member.object)? else {
            return Ok(None);
        };
        if member.optional && object.is_nullish() {
            return Ok(None);
        }
        let key = self.eval_expression(&member.expression)?;
        let key = to_property_key(&key, member.span)?;
        let value = self.get_property(&object, &key, member.span)?;
        Ok(Some(value))
    }

    fn eval_chain_call(
        &mut self,
        call: &'a oxc_ast::ast::CallExpression<'a>,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(callee) = self.eval_chain_value(&call.callee)? else {
            return Ok(None);
        };
        if call.optional && callee.is_nullish() {
            return Ok(None);
        }
        let arguments = self.eval_arguments(&call.arguments)?;
        let value = self.invoke_value(&callee, arguments, call.span)?;
        Ok(Some(value))
    }

    fn invoke_value(
        &mut self,
        callee: &Value,
        arguments: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match callee {
            Value::Function(id) => self.invoke_function(*id, arguments, span),
            _ => Err(RuntimeError::type_error(
                "Only functions are callable in CodeMode.",
                span,
            )),
        }
    }

    fn invoke_function(
        &mut self,
        id: FunctionId,
        arguments: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let function = self
            .functions
            .get(id.0)
            .cloned()
            .ok_or_else(|| RuntimeError::type_error("Unknown function reference.", span))?;
        let saved = Rc::clone(&self.scope);
        self.scope = Rc::new(RefCell::new(Scope::function(Some(Rc::clone(
            &function.closure,
        )))));
        let result = self.run_function(&function, arguments);
        self.scope = saved;
        result
    }

    fn run_function(
        &mut self,
        function: &FunctionDef<'a>,
        arguments: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        self.bind_parameters(function.params, arguments)?;
        match function.body {
            FunctionBodyDef::Block(body) => match self.eval_block(&body.statements)? {
                Flow::Return(value) => Ok(value),
                Flow::Normal => Ok(Value::Undefined),
                Flow::Break | Flow::Continue => Err(RuntimeError::new(
                    RuntimeErrorKind::ExecutionFailure,
                    "Illegal break or continue statement.",
                    body.span,
                )),
            },
            FunctionBodyDef::Expression(expression) => self.eval_expression(expression),
        }
    }

    fn bind_parameters(
        &mut self,
        parameters: &'a oxc_ast::ast::FormalParameters<'a>,
        arguments: Vec<Value>,
    ) -> Result<(), RuntimeError> {
        for (index, parameter) in parameters.items.iter().enumerate() {
            let mut value = arguments.get(index).cloned().unwrap_or(Value::Undefined);
            if value.is_undefined() {
                if let Some(initializer) = &parameter.initializer {
                    value = self.eval_expression(initializer)?;
                }
            }
            self.bind_pattern(&parameter.pattern, value, true, false)?;
        }

        if let Some(rest) = &parameters.rest {
            let values = arguments
                .iter()
                .skip(parameters.items.len())
                .cloned()
                .collect();
            self.bind_pattern(&rest.rest.argument, Value::array(values), true, false)?;
        }
        Ok(())
    }

    fn bind_pattern(
        &mut self,
        pattern: &'a BindingPattern<'a>,
        value: Value,
        mutable: bool,
        function_scoped: bool,
    ) -> Result<(), RuntimeError> {
        match pattern {
            BindingPattern::BindingIdentifier(identifier) => self.declare(
                identifier.name.as_str(),
                value,
                mutable,
                function_scoped,
                identifier.span,
            ),
            BindingPattern::AssignmentPattern(assignment) => {
                let value = if value.is_undefined() {
                    self.eval_expression(&assignment.right)?
                } else {
                    value
                };
                self.bind_pattern(&assignment.left, value, mutable, function_scoped)
            }
            BindingPattern::ObjectPattern(object) => {
                let source = match &value {
                    Value::Object(source) => Rc::clone(source),
                    _ => {
                        return Err(RuntimeError::new(
                            RuntimeErrorKind::InvalidDataValue,
                            "Object destructuring requires a data object value.",
                            object.span,
                        ));
                    }
                };
                let mut consumed = Vec::new();
                for property in &object.properties {
                    let key = if property.computed {
                        let key = self.eval_expression(property_key_expression(&property.key)?)?;
                        to_property_key(&key, property.key.span())?
                    } else {
                        static_property_key(&property.key)?
                    };
                    if is_blocked_member(&key) {
                        return Err(RuntimeError::type_error(
                            format!("Property '{key}' is not available in CodeMode."),
                            property.span,
                        ));
                    }
                    consumed.push(key.clone());
                    let value = source
                        .borrow()
                        .get(&key)
                        .cloned()
                        .unwrap_or(Value::Undefined);
                    self.bind_pattern(&property.value, value, mutable, function_scoped)?;
                }
                if let Some(rest) = &object.rest {
                    let mut remaining = IndexMap::new();
                    for (key, item) in source.borrow().iter() {
                        if !consumed.contains(key) && !is_blocked_member(key) {
                            remaining.insert(key.clone(), item.clone());
                        }
                    }
                    self.bind_pattern(
                        &rest.argument,
                        Value::Object(Rc::new(RefCell::new(remaining))),
                        mutable,
                        function_scoped,
                    )?;
                }
                Ok(())
            }
            BindingPattern::ArrayPattern(array) => {
                let source = match &value {
                    Value::Array(source) => Rc::clone(source),
                    _ => {
                        return Err(RuntimeError::new(
                            RuntimeErrorKind::InvalidDataValue,
                            "Array destructuring requires an array value.",
                            array.span,
                        ));
                    }
                };
                for (index, element) in array.elements.iter().enumerate() {
                    if let Some(element) = element {
                        let value = source
                            .borrow()
                            .get(index)
                            .cloned()
                            .unwrap_or(Value::Undefined);
                        self.bind_pattern(element, value, mutable, function_scoped)?;
                    }
                }
                if let Some(rest) = &array.rest {
                    let values = source
                        .borrow()
                        .iter()
                        .skip(array.elements.len())
                        .cloned()
                        .collect();
                    self.bind_pattern(
                        &rest.argument,
                        Value::array(values),
                        mutable,
                        function_scoped,
                    )?;
                }
                Ok(())
            }
        }
    }

    fn declare(
        &mut self,
        name: &str,
        value: Value,
        mutable: bool,
        function_scoped: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let scope = if function_scoped {
            self.function_scope()
        } else {
            Rc::clone(&self.scope)
        };
        let mut scope = scope.borrow_mut();
        if scope.bindings.contains_key(name) {
            return Err(RuntimeError::reference(
                format!("Identifier '{name}' has already been declared."),
                span,
            ));
        }
        scope
            .bindings
            .insert(name.to_owned(), Binding { value, mutable });
        Ok(())
    }

    fn function_scope(&self) -> Rc<RefCell<Scope>> {
        let mut scope = Rc::clone(&self.scope);
        loop {
            let parent = {
                let current = scope.borrow();
                if current.function_scope {
                    return Rc::clone(&scope);
                }
                current.parent.clone()
            };
            match parent {
                Some(parent) => scope = parent,
                None => return scope,
            }
        }
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        let mut scope = Rc::clone(&self.scope);
        loop {
            let (value, parent) = {
                let current = scope.borrow();
                (
                    current
                        .bindings
                        .get(name)
                        .map(|binding| binding.value.clone()),
                    current.parent.clone(),
                )
            };
            if let Some(value) = value {
                return Some(value);
            }
            let parent = parent?;
            scope = parent;
        }
    }

    fn assign_identifier(
        &mut self,
        name: &str,
        value: Value,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let mut scope = Rc::clone(&self.scope);
        loop {
            let parent = {
                let mut current = scope.borrow_mut();
                if let Some(binding) = current.bindings.get_mut(name) {
                    if !binding.mutable {
                        return Err(RuntimeError::type_error(
                            format!("Assignment to constant variable '{name}'."),
                            span,
                        ));
                    }
                    binding.value = value;
                    return Ok(());
                }
                current.parent.clone()
            };
            match parent {
                Some(parent) => scope = parent,
                None => {
                    return Err(RuntimeError::reference(
                        format!("Unknown identifier '{name}'."),
                        span,
                    ));
                }
            }
        }
    }

    fn read_target(&mut self, target: &'a AssignmentTarget<'a>) -> Result<Value, RuntimeError> {
        let simple = target.as_simple_assignment_target().ok_or_else(|| {
            RuntimeError::type_error(
                "Assignment target must be an identifier or member expression.",
                target.span(),
            )
        })?;
        self.read_simple_target(simple)
    }

    fn read_simple_target(
        &mut self,
        target: &'a SimpleAssignmentTarget<'a>,
    ) -> Result<Value, RuntimeError> {
        match target {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                self.lookup(identifier.name.as_str()).ok_or_else(|| {
                    RuntimeError::reference(
                        format!("Unknown identifier '{}'.", identifier.name),
                        identifier.span,
                    )
                })
            }
            SimpleAssignmentTarget::TSAsExpression(expression) => {
                self.eval_expression(&expression.expression)
            }
            SimpleAssignmentTarget::TSNonNullExpression(expression) => {
                self.eval_expression(&expression.expression)
            }
            SimpleAssignmentTarget::TSSatisfiesExpression(expression) => {
                self.eval_expression(&expression.expression)
            }
            SimpleAssignmentTarget::TSTypeAssertion(expression) => {
                self.eval_expression(&expression.expression)
            }
            _ => match target.as_member_expression() {
                Some(member) => self.eval_member(member),
                None => Err(RuntimeError::type_error(
                    "Assignment target must be an identifier or member expression.",
                    target.span(),
                )),
            },
        }
    }

    fn assign_target(
        &mut self,
        target: &'a AssignmentTarget<'a>,
        value: Value,
    ) -> Result<(), RuntimeError> {
        let simple = target.as_simple_assignment_target().ok_or_else(|| {
            RuntimeError::type_error(
                "Assignment target must be an identifier or member expression.",
                target.span(),
            )
        })?;
        self.assign_simple_target(simple, value)
    }

    fn assign_simple_target(
        &mut self,
        target: &'a SimpleAssignmentTarget<'a>,
        value: Value,
    ) -> Result<(), RuntimeError> {
        match target {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                self.assign_identifier(identifier.name.as_str(), value, identifier.span)
            }
            SimpleAssignmentTarget::TSAsExpression(expression) => {
                self.assign_expression(&expression.expression, value)
            }
            SimpleAssignmentTarget::TSNonNullExpression(expression) => {
                self.assign_expression(&expression.expression, value)
            }
            SimpleAssignmentTarget::TSSatisfiesExpression(expression) => {
                self.assign_expression(&expression.expression, value)
            }
            SimpleAssignmentTarget::TSTypeAssertion(expression) => {
                self.assign_expression(&expression.expression, value)
            }
            _ => match target.as_member_expression() {
                Some(member) => self.write_member(member, value),
                None => Err(RuntimeError::type_error(
                    "Assignment target must be an identifier or member expression.",
                    target.span(),
                )),
            },
        }
    }

    fn assign_expression(
        &mut self,
        expression: &'a Expression<'a>,
        value: Value,
    ) -> Result<(), RuntimeError> {
        if let Expression::Identifier(identifier) = expression {
            return self.assign_identifier(identifier.name.as_str(), value, identifier.span);
        }
        if let Some(member) = expression.as_member_expression() {
            return self.write_member(member, value);
        }
        Err(RuntimeError::type_error(
            "Assignment target must be an identifier or member expression.",
            expression.span(),
        ))
    }

    fn write_member(
        &mut self,
        member: &'a MemberExpression<'a>,
        value: Value,
    ) -> Result<(), RuntimeError> {
        match member {
            MemberExpression::StaticMemberExpression(member) => {
                let object = self.eval_expression(&member.object)?;
                self.set_property(&object, member.property.name.as_str(), value, member.span)
            }
            MemberExpression::ComputedMemberExpression(member) => {
                let object = self.eval_expression(&member.object)?;
                let key = self.eval_expression(&member.expression)?;
                let key = to_property_key(&key, member.span)?;
                self.set_property(&object, &key, value, member.span)
            }
            MemberExpression::PrivateFieldExpression(member) => Err(RuntimeError::unsupported(
                "PrivateFieldExpression",
                member.span,
            )),
        }
    }

    fn get_property(&self, object: &Value, key: &str, span: Span) -> Result<Value, RuntimeError> {
        if is_blocked_member(key) {
            return Err(RuntimeError::type_error(
                format!("Property '{key}' is not available in CodeMode."),
                span,
            ));
        }
        match object {
            Value::Object(object) => Ok(object
                .borrow()
                .get(key)
                .cloned()
                .unwrap_or(Value::Undefined)),
            Value::Array(array) => {
                if key == "length" {
                    return Ok(Value::Number(array.borrow().len() as f64));
                }
                match key.parse::<usize>() {
                    Ok(index) => Ok(array
                        .borrow()
                        .get(index)
                        .cloned()
                        .unwrap_or(Value::Undefined)),
                    Err(_) => Ok(Value::Undefined),
                }
            }
            Value::String(value) => {
                if key == "length" {
                    return Ok(Value::Number(value.chars().count() as f64));
                }
                match key.parse::<usize>() {
                    Ok(index) => Ok(value
                        .chars()
                        .nth(index)
                        .map_or(Value::Undefined, |character| {
                            Value::String(character.to_string())
                        })),
                    Err(_) => Ok(Value::Undefined),
                }
            }
            Value::Null | Value::Undefined => Err(RuntimeError::type_error(
                "Cannot read a property of null or undefined.",
                span,
            )),
            _ => Ok(Value::Undefined),
        }
    }

    fn set_property(
        &self,
        object: &Value,
        key: &str,
        value: Value,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if is_blocked_member(key) {
            return Err(RuntimeError::type_error(
                format!("Property '{key}' is not available in CodeMode."),
                span,
            ));
        }
        match object {
            Value::Object(object) => {
                object.borrow_mut().insert(key.to_owned(), value);
                Ok(())
            }
            Value::Array(array) => match key.parse::<usize>() {
                Ok(index) => {
                    let mut array = array.borrow_mut();
                    if index >= array.len() {
                        array.resize(index + 1, Value::Undefined);
                    }
                    array[index] = value;
                    Ok(())
                }
                Err(_) => Err(RuntimeError::type_error(
                    "Array property name must be an index or 'length'.",
                    span,
                )),
            },
            _ => Err(RuntimeError::type_error(
                "Assignment target must be a data object or array.",
                span,
            )),
        }
    }
}

/// Parses and evaluates a program.
pub fn execute<'a>(program: &'a Program<'a>) -> Result<Value, RuntimeError> {
    let mut interpreter = Interpreter::<'a>::new();
    interpreter.run(program)
}

fn property_key_expression<'a, 'b>(
    key: &'b PropertyKey<'a>,
) -> Result<&'b Expression<'a>, RuntimeError> {
    key.as_expression()
        .ok_or_else(|| RuntimeError::unsupported("ObjectExpression", key.span()))
}

fn static_property_key(key: &PropertyKey<'_>) -> Result<String, RuntimeError> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Ok(identifier.name.as_str().to_owned()),
        _ => match key.as_expression() {
            Some(Expression::StringLiteral(literal)) => Ok(literal.value.as_str().to_owned()),
            Some(Expression::NumericLiteral(literal)) => Ok(format_number(literal.value)),
            _ => Err(RuntimeError::unsupported("PropertyKey", key.span())),
        },
    }
}

fn spread_items(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(array) => Some(array.borrow().clone()),
        Value::String(value) => Some(
            value
                .chars()
                .map(|c| Value::String(c.to_string()))
                .collect(),
        ),
        _ => None,
    }
}

fn iterable_items(value: &Value, span: Span) -> Result<Vec<Value>, RuntimeError> {
    spread_items(value).ok_or_else(|| {
        RuntimeError::type_error("Loop target must be an array or string in CodeMode.", span)
    })
}

fn enumerable_keys(value: &Value) -> Vec<Value> {
    match value {
        Value::Object(object) => object
            .borrow()
            .keys()
            .map(|key| Value::String(key.clone()))
            .collect(),
        Value::Array(array) => (0..array.borrow().len())
            .map(|index| Value::String(index.to_string()))
            .collect(),
        Value::String(value) => (0..value.chars().count())
            .map(|index| Value::String(index.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

fn has_property(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => object.borrow().contains_key(key),
        Value::Array(array) => {
            key == "length"
                || key
                    .parse::<usize>()
                    .is_ok_and(|index| index < array.borrow().len())
        }
        Value::String(value) => {
            key == "length"
                || key
                    .parse::<usize>()
                    .is_ok_and(|index| index < value.chars().count())
        }
        _ => false,
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Boolean(value) => *value,
        Value::Number(value) => *value != 0.0 && !value.is_nan(),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) | Value::Function(_) => true,
    }
}

fn strict_equals(left: &Value, right: &Value) -> bool {
    left == right
}

fn loose_equals(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null | Value::Undefined, Value::Null | Value::Undefined) => true,
        (Value::Number(left), Value::Number(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        (Value::Number(left), Value::String(right)) => {
            *left == to_number(&Value::String(right.clone()))
        }
        (Value::String(left), Value::Number(right)) => {
            to_number(&Value::String(left.clone())) == *right
        }
        (Value::Boolean(_), _) => loose_equals(&Value::Number(to_number(left)), right),
        (_, Value::Boolean(_)) => loose_equals(left, &Value::Number(to_number(right))),
        _ => left == right,
    }
}

fn compare(left: &Value, right: &Value, _span: Span) -> Result<f64, RuntimeError> {
    match (left, right) {
        (Value::String(left), Value::String(right)) => Ok(if left < right {
            -1.0
        } else if left > right {
            1.0
        } else {
            0.0
        }),
        _ => {
            let left = to_number(left);
            let right = to_number(right);
            Ok(if left < right {
                -1.0
            } else if left > right {
                1.0
            } else {
                0.0
            })
        }
    }
}

fn to_number(value: &Value) -> f64 {
    match value {
        Value::Undefined => f64::NAN,
        Value::Null => 0.0,
        Value::Boolean(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(value) => *value,
        Value::String(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Array(array) => to_number(&Value::String(
            array
                .borrow()
                .iter()
                .map(|item| {
                    if item.is_nullish() {
                        String::new()
                    } else {
                        to_js_string(item)
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
        )),
        Value::Object(_) | Value::Function(_) => f64::NAN,
    }
}

fn to_int32(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let value = value.trunc() % 4_294_967_296.0;
    let value = if value < 0.0 {
        value + 4_294_967_296.0
    } else {
        value
    };
    value as u32 as i32
}

fn to_uint32(value: f64) -> u32 {
    to_int32(value) as u32
}

fn to_js_string(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".to_owned(),
        Value::Null => "null".to_owned(),
        Value::Boolean(value) => value.to_string(),
        Value::Number(value) => format_number(*value),
        Value::String(value) => value.clone(),
        Value::Array(array) => array
            .borrow()
            .iter()
            .map(|item| {
                if item.is_nullish() {
                    String::new()
                } else {
                    to_js_string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => value.to_json().to_string(),
        Value::Function(_) => "[Function]".to_owned(),
    }
}

fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Infinity".to_owned()
        } else {
            "Infinity".to_owned()
        };
    }
    value.to_string()
}

fn to_property_key(value: &Value, span: Span) -> Result<String, RuntimeError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(format_number(*value)),
        _ => Err(RuntimeError::type_error(
            "Property key must be a string or number.",
            span,
        )),
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Undefined => "undefined",
        Value::Null => "object",
        Value::Boolean(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) | Value::Object(_) => "object",
        Value::Function(_) => "function",
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
        Expression::NewExpression(_) => "NewExpression",
        _ => "Expression",
    }
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;

    use super::{execute, RuntimeError};
    use crate::{parse_program, Value};

    fn run(source: &str) -> Result<Value, RuntimeError> {
        let allocator = Allocator::default();
        let program = parse_program(&allocator, source).expect("fixture should parse");
        execute(&program)
    }

    fn number(value: f64) -> Value {
        Value::Number(value)
    }

    fn string(value: &str) -> Value {
        Value::String(value.to_owned())
    }

    fn run_value(source: &str) -> Value {
        run(source).expect("program should run")
    }

    #[test]
    fn evaluates_arithmetic_and_compound_assignment() {
        assert_eq!(
            run_value("const a = 1; let b = 2; b += 3; return a + b;"),
            number(6.0)
        );
    }

    #[test]
    fn evaluates_loops_and_control_flow() {
        let source = r#"
            let total = 0;
            for (let index = 0; index < 10; index += 1) {
                if (index === 3) continue;
                if (index === 7) break;
                total += index;
            }
            return total;
        "#;
        assert_eq!(run_value(source), number(18.0));
    }

    #[test]
    fn evaluates_functions_defaults_and_closures() {
        let source = r#"
            function makeAdder(offset) {
                return (value) => value + offset;
            }
            const addTwo = makeAdder(2);
            function withDefault(value, extra = 1) {
                return value + extra;
            }
            return [addTwo(3), withDefault(4)].length;
        "#;
        assert_eq!(run_value(source), number(2.0));
    }

    #[test]
    fn aliases_objects_and_preserves_identity() {
        let source = r#"
            const first = { x: 1 };
            const second = first;
            second.x = 2;
            const fresh = { x: 2 };
            return [first.x, first === second, first === fresh];
        "#;
        let value = run(source).expect("program should run");
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], number(2.0));
        assert_eq!(array[1], Value::Boolean(true));
        assert_eq!(array[2], Value::Boolean(false));
    }

    #[test]
    fn evaluates_destructuring_defaults_and_rest() {
        let source = r#"
            const { a, b: [c, ...rest] = [2, 3, 4], missing = 9 } = { a: 1 };
            return [a, c, rest.length, missing];
        "#;
        let value = run(source).expect("program should run");
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], number(1.0));
        assert_eq!(array[1], number(2.0));
        assert_eq!(array[2], number(2.0));
        assert_eq!(array[3], number(9.0));
    }

    #[test]
    fn evaluates_try_catch_finally() {
        let source = r#"
            let marker = 0;
            try {
                throw "failure";
            } catch (error) {
                marker = 1;
            } finally {
                marker += 1;
            }
            return marker;
        "#;
        assert_eq!(run_value(source), number(2.0));
    }

    #[test]
    fn evaluates_optional_chaining_and_nullish_coalescing() {
        assert_eq!(
            run_value("const source = null; return source?.value ?? 5;"),
            number(5.0)
        );
    }

    #[test]
    fn evaluates_switch_fallthrough() {
        let source = r#"
            let result = "";
            switch (2) {
                case 1: result += "a";
                case 2: result += "b";
                case 3: result += "c"; break;
                default: result += "d";
            }
            return result;
        "#;
        assert_eq!(run_value(source), string("bc"));
    }

    #[test]
    fn reports_unknown_identifiers() {
        let error = run("return missing;").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::ReferenceError);
    }

    #[test]
    fn blocks_prototype_members() {
        let error =
            run(r#"const value = {}; return value["__proto__"];"#).expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
    }

    #[test]
    fn typeof_undeclared_identifier_is_string_undefined() {
        assert_eq!(run_value("return typeof missing;"), string("undefined"));
    }

    #[test]
    fn evaluates_for_of_and_for_in() {
        let source = r#"
            const values = [];
            for (const item of [1, 2, 3]) values[values.length] = item;
            const keys = [];
            for (const key in { a: 1, b: 2 }) keys[keys.length] = key;
            return [values.length, values[2], keys.length, keys[1]];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], number(3.0));
        assert_eq!(array[1], number(3.0));
        assert_eq!(array[2], number(2.0));
        assert_eq!(array[3], string("b"));
    }

    #[test]
    fn evaluates_typescript_only_syntax() {
        let source = r#"
            interface Row { id: number }
            const row = { id: 1 } as Row;
            const id: number = row.id!;
            return id satisfies number;
        "#;
        assert_eq!(run_value(source), number(1.0));
    }

    #[test]
    fn rejects_unsupported_new_expressions() {
        let error = run("return new Date();").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::UnsupportedSyntax);
    }

    #[test]
    fn rejects_unsupported_unary_operators() {
        let error = run("return void 0;").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::UnsupportedSyntax);
    }
}
