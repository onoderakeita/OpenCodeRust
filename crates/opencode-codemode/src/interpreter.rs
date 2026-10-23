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
use std::time::{Duration, Instant};

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

use crate::limits::ToolCallBudget;
use crate::tools::{ToolError, ToolHost};
use crate::value::{
    check_data, is_blocked_member, ordered_keys, ErrorKind, FunctionId, MapValue, PromiseState,
    SetValue, Value,
};

/// Maximum nested user-function calls before the interpreter raises a
/// `RangeError`. The ceiling is conservative because Rust evaluation frames are
/// much larger than JavaScript frames.
const MAX_CALL_DEPTH: usize = 64;

/// Maximum nested statement or expression evaluations. This protects the Rust
/// stack from parser-accepted, deeply nested syntax.
const MAX_EVAL_DEPTH: usize = 64;

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
    /// Invalid JSON or other syntax failure.
    SyntaxError,
    /// Malformed percent-encoded URI data.
    URIError,
    /// A host value that is not plain data.
    InvalidDataValue,
    /// Syntax accepted by the parser but outside the interpreter subset.
    UnsupportedSyntax,
    /// The wall-clock timeout elapsed.
    Timeout,
    /// The configured tool-call budget was exhausted.
    ToolCallLimitExceeded,
    /// A host tool failed.
    ToolFailure,
    /// Any other execution failure.
    ExecutionFailure,
}

impl Display for RuntimeErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReferenceError => "ReferenceError",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::SyntaxError => "SyntaxError",
            Self::URIError => "URIError",
            Self::InvalidDataValue => "InvalidDataValue",
            Self::UnsupportedSyntax => "UnsupportedSyntax",
            Self::Timeout => "Timeout",
            Self::ToolCallLimitExceeded => "ToolCallLimitExceeded",
            Self::ToolFailure => "ToolFailure",
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

    fn tool_failure(message: impl Into<String>, value: Value, span: Span) -> Self {
        Self {
            kind: RuntimeErrorKind::ToolFailure,
            message: message.into(),
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
            let kind = match self.kind {
                RuntimeErrorKind::ReferenceError => ErrorKind::ReferenceError,
                RuntimeErrorKind::TypeError => ErrorKind::TypeError,
                RuntimeErrorKind::RangeError => ErrorKind::RangeError,
                RuntimeErrorKind::SyntaxError => ErrorKind::SyntaxError,
                RuntimeErrorKind::URIError => ErrorKind::URIError,
                _ => ErrorKind::Error,
            };
            Value::error(kind, self.message.clone())
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

    /// Top-level program scope: variable declarations shadow installed globals.
    fn script(parent: Rc<RefCell<Scope>>) -> Self {
        Self {
            parent: Some(parent),
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
    logs: Vec<String>,
    tool_host: Option<Box<dyn ToolHost>>,
    tool_budget: ToolCallBudget,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    call_depth: usize,
    eval_depth: usize,
}

impl<'a> Default for Interpreter<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Interpreter<'a> {
    /// Creates an interpreter with a fresh global scope and the standard
    /// globals installed.
    pub fn new() -> Self {
        let globals = Rc::new(RefCell::new(Scope::global()));
        let mut interpreter = Self {
            functions: Vec::new(),
            scope: Rc::clone(&globals),
            logs: Vec::new(),
            tool_host: None,
            tool_budget: ToolCallBudget::new(None),
            timeout: None,
            deadline: None,
            call_depth: 0,
            eval_depth: 0,
        };
        interpreter.install_globals();
        interpreter.scope = Rc::new(RefCell::new(Scope::script(globals)));
        interpreter
    }

    /// Log lines captured from `console` calls, in emission order.
    pub fn logs(&self) -> &[String] {
        &self.logs
    }

    /// Installs the host consulted for `tools.*` references.
    pub fn set_tool_host(&mut self, host: Box<dyn ToolHost>) {
        self.tool_host = Some(host);
    }

    /// Applies an optional tool-call budget to this interpreter.
    ///
    /// Calls are recorded even without a budget so callers can report the
    /// invocation order.
    pub fn set_tool_call_limit(&mut self, limit: Option<usize>) {
        self.tool_budget = ToolCallBudget::new(limit);
    }

    /// Applies the tool-call and timeout portions of an execution limit set.
    ///
    /// Output bounds are applied by the caller because they depend on how the
    /// result is serialized for the model.
    pub fn set_limits(&mut self, limits: &crate::limits::ExecutionLimits) {
        self.tool_budget = ToolCallBudget::new(limits.max_tool_calls);
        self.timeout = limits.timeout_ms.map(Duration::from_millis);
    }

    fn enter_eval(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.eval_depth >= MAX_EVAL_DEPTH {
            return Err(RuntimeError::new(
                RuntimeErrorKind::RangeError,
                format!("Maximum evaluation depth of {MAX_EVAL_DEPTH} exceeded."),
                span,
            ));
        }
        self.eval_depth += 1;
        Ok(())
    }

    fn check_timeout(&self, span: Span) -> Result<(), RuntimeError> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Timeout,
                "Execution timed out.",
                span,
            ));
        }
        Ok(())
    }

    /// Admitted tool calls in invocation order.
    pub fn tool_calls(&self) -> &[String] {
        self.tool_budget.calls()
    }

    fn install_globals(&mut self) {
        let console = Value::from_entries([
            ("log", Value::Builtin("console.log")),
            ("info", Value::Builtin("console.info")),
            ("warn", Value::Builtin("console.warn")),
            ("error", Value::Builtin("console.error")),
            ("debug", Value::Builtin("console.debug")),
            ("dir", Value::Builtin("console.dir")),
            ("table", Value::Builtin("console.table")),
        ]);
        let math = Value::from_entries([
            ("abs", Value::Builtin("Math.abs")),
            ("cbrt", Value::Builtin("Math.cbrt")),
            ("hypot", Value::Builtin("Math.hypot")),
            ("log2", Value::Builtin("Math.log2")),
            ("log10", Value::Builtin("Math.log10")),
            ("ceil", Value::Builtin("Math.ceil")),
            ("floor", Value::Builtin("Math.floor")),
            ("round", Value::Builtin("Math.round")),
            ("trunc", Value::Builtin("Math.trunc")),
            ("max", Value::Builtin("Math.max")),
            ("min", Value::Builtin("Math.min")),
            ("pow", Value::Builtin("Math.pow")),
            ("sqrt", Value::Builtin("Math.sqrt")),
            ("sign", Value::Builtin("Math.sign")),
            ("log", Value::Builtin("Math.log")),
            ("exp", Value::Builtin("Math.exp")),
            ("sin", Value::Builtin("Math.sin")),
            ("cos", Value::Builtin("Math.cos")),
            ("tan", Value::Builtin("Math.tan")),
            ("PI", Value::Number(std::f64::consts::PI)),
            ("E", Value::Number(std::f64::consts::E)),
            ("LN2", Value::Number(std::f64::consts::LN_2)),
            ("LN10", Value::Number(std::f64::consts::LN_10)),
            ("LOG2E", Value::Number(std::f64::consts::LOG2_E)),
            ("LOG10E", Value::Number(std::f64::consts::LOG10_E)),
            ("SQRT2", Value::Number(std::f64::consts::SQRT_2)),
            ("SQRT1_2", Value::Number(std::f64::consts::FRAC_1_SQRT_2)),
        ]);
        let json = Value::from_entries([
            ("parse", Value::Builtin("JSON.parse")),
            ("stringify", Value::Builtin("JSON.stringify")),
        ]);
        let mut scope = self.scope.borrow_mut();
        for (name, value) in [
            ("console", console),
            ("Math", math),
            ("JSON", json),
            ("Object", Value::Builtin("Object")),
            ("Array", Value::Builtin("Array")),
            ("Promise", Value::Builtin("Promise")),
            ("tools", Value::tool_ref(Vec::new())),
            ("Error", Value::Builtin("Error")),
            ("TypeError", Value::Builtin("TypeError")),
            ("RangeError", Value::Builtin("RangeError")),
            ("SyntaxError", Value::Builtin("SyntaxError")),
            ("ReferenceError", Value::Builtin("ReferenceError")),
            ("EvalError", Value::Builtin("EvalError")),
            ("URIError", Value::Builtin("URIError")),
            ("Map", Value::Builtin("Map")),
            ("Set", Value::Builtin("Set")),
            ("Date", Value::Builtin("Date")),
            ("undefined", Value::Undefined),
            ("NaN", Value::Number(f64::NAN)),
            ("Infinity", Value::Number(f64::INFINITY)),
            ("String", Value::Builtin("String")),
            ("Number", Value::Builtin("Number")),
            ("Boolean", Value::Builtin("Boolean")),
            ("parseInt", Value::Builtin("parseInt")),
            ("parseFloat", Value::Builtin("parseFloat")),
            ("isNaN", Value::Builtin("isNaN")),
            ("isFinite", Value::Builtin("isFinite")),
            ("encodeURI", Value::Builtin("encodeURI")),
            ("encodeURIComponent", Value::Builtin("encodeURIComponent")),
            ("decodeURI", Value::Builtin("decodeURI")),
            ("decodeURIComponent", Value::Builtin("decodeURIComponent")),
        ] {
            scope.bindings.insert(
                name.to_owned(),
                Binding {
                    value,
                    mutable: false,
                },
            );
        }
    }

    /// Evaluates a program and returns its top-level `return` value, or
    /// `undefined`.
    pub fn run(&mut self, program: &'a Program<'a>) -> Result<Value, RuntimeError> {
        self.deadline = self
            .timeout
            .and_then(|timeout| Instant::now().checked_add(timeout));
        self.hoist_functions(&program.body)?;
        for statement in &program.body {
            match self.eval_statement(statement)? {
                Flow::Normal => {}
                Flow::Return(value) => {
                    check_data(&value, "result").map_err(|error| {
                        RuntimeError::new(
                            RuntimeErrorKind::InvalidDataValue,
                            error.to_string(),
                            statement.span(),
                        )
                    })?;
                    return Ok(value);
                }
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
        self.enter_eval(statement.span())?;
        let result = self.eval_statement_inner(statement);
        self.eval_depth -= 1;
        result
    }

    fn eval_statement_inner(&mut self, statement: &'a Statement<'a>) -> Result<Flow, RuntimeError> {
        self.check_timeout(statement.span())?;
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
                let keys = if let Some(path) = value.tool_path() {
                    self.tool_host
                        .as_ref()
                        .map(|host| host.keys(path))
                        .unwrap_or_default()
                        .into_iter()
                        .map(Value::String)
                        .collect()
                } else {
                    enumerable_keys(&value, statement.right.span())?
                };
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
        self.check_timeout(statements.first().map_or(Span::default(), GetSpan::span))?;
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
        self.enter_eval(expression.span())?;
        let result = self.eval_expression_inner(expression);
        self.eval_depth -= 1;
        result
    }

    fn eval_expression_inner(
        &mut self,
        expression: &'a Expression<'a>,
    ) -> Result<Value, RuntimeError> {
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
                                    let source = source.borrow();
                                    for key in ordered_keys(&source) {
                                        if is_blocked_member(&key) {
                                            return Err(RuntimeError::type_error(
                                                format!("Property '{key}' is not available in CodeMode."),
                                                spread.span,
                                            ));
                                        }
                                        let value =
                                            source.get(&key).expect("ordered keys exist").clone();
                                        values.insert(key, value);
                                    }
                                }
                                Value::Error(error) => {
                                    values.insert(
                                        "name".to_owned(),
                                        Value::String(error.kind.name().to_owned()),
                                    );
                                    values.insert(
                                        "message".to_owned(),
                                        Value::String(error.message.clone()),
                                    );
                                }
                                // Sandbox values have no own enumerable properties in
                                // JS, so spreading them is a no-op. A promise is
                                // interpreter machinery and must be awaited first.
                                Value::Date(_) | Value::Map(_) | Value::Set(_) => {}
                                _ => {
                                    return Err(RuntimeError::new(
                                        RuntimeErrorKind::InvalidDataValue,
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
            Expression::AwaitExpression(expression) => {
                let value = self.eval_expression(&expression.argument)?;
                match value {
                    Value::Promise(promise) => match &*promise {
                        PromiseState::Fulfilled(value) => Ok(value.clone()),
                        PromiseState::Rejected(reason) => {
                            Err(RuntimeError::thrown(reason.clone(), expression.span))
                        }
                    },
                    other => Ok(other),
                }
            }
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
            Expression::NewExpression(expression) => {
                let callee = self.eval_expression(&expression.callee)?;
                let arguments = self.eval_arguments(&expression.arguments)?;
                match callee {
                    Value::Builtin("Map") => construct_map(arguments.first(), expression.span),
                    Value::Builtin("Set") => construct_set(arguments.first(), expression.span),
                    Value::Builtin("Date") => Ok(construct_date(&arguments)),
                    Value::Builtin(name) if ErrorKind::from_name(name).is_some() => {
                        Ok(make_error(name, &arguments))
                    }
                    _ => Err(RuntimeError::unsupported("new", expression.span)),
                }
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
        // `instanceof` observes operands without coercing them, so it is the
        // only operator that accepts functions and un-awaited promises.
        if !matches!(operator, BinaryOperator::Instanceof) {
            let mut seen = Vec::new();
            if contains_opaque_reference(left, &mut seen)
                || contains_opaque_reference(right, &mut seen)
            {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::InvalidDataValue,
                    "Binary operators require data values in CodeMode. Await promises \
                     before using them in expressions.",
                    span,
                ));
            }
        }
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
                let left = addition_operand(left);
                let right = addition_operand(right);
                if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
                    Ok(Value::String(format!(
                        "{}{}",
                        to_js_string(&left),
                        to_js_string(&right)
                    )))
                } else {
                    Ok(Value::Number(to_number(&left) + to_number(&right)))
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
                if !matches!(
                    right,
                    Value::Object(_)
                        | Value::Array(_)
                        | Value::Date(_)
                        | Value::Error(_)
                        | Value::Map(_)
                        | Value::Set(_)
                        | Value::Promise(_)
                ) {
                    return Err(RuntimeError::type_error(
                        "The 'in' operator requires a data object on the right-hand side.",
                        span,
                    ));
                }
                let key = to_property_key(left, span)?;
                Ok(Value::Boolean(has_property(right, &key)))
            }
            BinaryOperator::Instanceof => Ok(Value::Boolean(instance_of(left, right, span)?)),
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
            Value::Builtin(name) => self.call_builtin(name, &arguments, span),
            Value::BoundMethod { receiver, method } => {
                self.call_method(method, receiver, &arguments, span)
            }
            Value::ToolRef(path) => self.invoke_tool(path, arguments, span),
            _ => Err(RuntimeError::type_error(
                "Only functions are callable in CodeMode.",
                span,
            )),
        }
    }

    fn call_builtin(
        &mut self,
        name: &'static str,
        arguments: &[Value],
        _span: Span,
    ) -> Result<Value, RuntimeError> {
        let argument = |index: usize| arguments.get(index).cloned().unwrap_or(Value::Undefined);
        // Upstream coercion builtins run their input through the data boundary
        // before coercing, so callables and un-awaited promises are rejected
        // instead of stringifying to "[Function]" or "[object Promise]".
        if matches!(
            name,
            "String"
                | "Number"
                | "Boolean"
                | "parseInt"
                | "parseFloat"
                | "encodeURI"
                | "encodeURIComponent"
                | "decodeURI"
                | "decodeURIComponent"
        ) {
            match coercion_issue(&argument(0), &mut Vec::new()) {
                Some(CoercionIssue::Promise) => {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::InvalidDataValue,
                        format!("{name} input contains an un-awaited Promise; await it first."),
                        _span,
                    ))
                }
                Some(CoercionIssue::NonData) => {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::InvalidDataValue,
                        format!("{name} input must contain data only."),
                        _span,
                    ))
                }
                None => {}
            }
        }
        match name {
            "String" => Ok(Value::String(to_js_string(&argument(0)))),
            "Number" => Ok(Value::Number(to_number(&argument(0)))),
            "Boolean" => Ok(Value::Boolean(is_truthy(&argument(0)))),
            "parseInt" => Ok(Value::Number(parse_int(
                &to_js_string(&argument(0)),
                to_number(&argument(1)),
            ))),
            "parseFloat" => Ok(Value::Number(parse_float(&to_js_string(&argument(0))))),
            "isNaN" => Ok(Value::Boolean(to_number(&argument(0)).is_nan())),
            "isFinite" => Ok(Value::Boolean(to_number(&argument(0)).is_finite())),
            "encodeURI" | "encodeURIComponent" | "decodeURI" | "decodeURIComponent" => {
                let text = to_js_string(&argument(0));
                let result = match name {
                    "encodeURI" => Ok(crate::uri::encode(&text, true)),
                    "encodeURIComponent" => Ok(crate::uri::encode(&text, false)),
                    "decodeURI" => crate::uri::decode(&text, true),
                    _ => crate::uri::decode(&text, false),
                };
                result.map(Value::String).map_err(|()| {
                    RuntimeError::new(
                        RuntimeErrorKind::URIError,
                        format!("{name} received malformed URI data: URI malformed"),
                        _span,
                    )
                })
            }
            "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError"
            | "EvalError" | "URIError" => Ok(make_error(name, arguments)),
            "Map" | "Set" | "Date" => Err(RuntimeError::type_error(
                format!("Constructor {name} requires 'new'."),
                _span,
            )),
            "Date.now" => Ok(Value::Number(crate::date::now_ms())),
            "Date.parse" => Ok(Value::Number(crate::date::parse_iso(&to_js_string(
                &argument(0),
            )))),
            "Date.UTC" => Ok(Value::Number(crate::date::from_components(
                &arguments.iter().map(to_number).collect::<Vec<_>>(),
            ))),
            "Number.isNaN" => Ok(Value::Boolean(
                matches!(argument(0), Value::Number(value) if value.is_nan()),
            )),
            "Number.isFinite" => Ok(Value::Boolean(
                matches!(argument(0), Value::Number(value) if value.is_finite()),
            )),
            "Number.isInteger" => Ok(Value::Boolean(
                matches!(argument(0), Value::Number(value) if value.is_finite() && value.fract() == 0.0),
            )),
            "Number.isSafeInteger" => {
                Ok(Value::Boolean(matches!(argument(0), Value::Number(value)
                    if value.is_finite()
                        && value.fract() == 0.0
                        && value.abs() <= 9_007_199_254_740_991.0)))
            }
            "Number.parseInt" => Ok(Value::Number(parse_int(
                &to_js_string(&argument(0)),
                to_number(&argument(1)),
            ))),
            "Number.parseFloat" => Ok(Value::Number(parse_float(&to_js_string(&argument(0))))),
            "String.fromCharCode" => {
                let mut text = String::new();
                for value in arguments {
                    let Value::Number(code) = value else {
                        return Err(RuntimeError::type_error(
                            "String.fromCharCode expects number arguments.",
                            _span,
                        ));
                    };
                    let code = if code.is_finite() {
                        code.trunc().rem_euclid(65_536.0) as u32
                    } else {
                        0
                    };
                    text.push(char::from_u32(code).unwrap_or('\0'));
                }
                Ok(Value::String(text))
            }
            "String.fromCodePoint" => {
                let mut text = String::new();
                for value in arguments {
                    let Value::Number(code) = value else {
                        return Err(RuntimeError::type_error(
                            "String.fromCodePoint expects number arguments.",
                            _span,
                        ));
                    };
                    let character = if code.is_finite()
                        && code.fract() == 0.0
                        && (0.0..=1_114_111.0).contains(code)
                    {
                        char::from_u32(*code as u32)
                    } else {
                        None
                    };
                    let Some(character) = character else {
                        return Err(RuntimeError::new(
                            RuntimeErrorKind::RangeError,
                            format!("Invalid code point: {code}."),
                            _span,
                        ));
                    };
                    text.push(character);
                }
                Ok(Value::String(text))
            }
            "Math.abs" => Ok(Value::Number(to_number(&argument(0)).abs())),
            "Math.cbrt" => Ok(Value::Number(to_number(&argument(0)).cbrt())),
            "Math.hypot" => Ok(Value::Number(math_hypot(arguments))),
            "Math.log2" => Ok(Value::Number(to_number(&argument(0)).log2())),
            "Math.log10" => Ok(Value::Number(to_number(&argument(0)).log10())),
            "Math.ceil" => Ok(Value::Number(to_number(&argument(0)).ceil())),
            "Math.floor" => Ok(Value::Number(to_number(&argument(0)).floor())),
            "Math.round" => {
                let value = to_number(&argument(0));
                // ECMAScript rounds x in (0, 0.5) to +0. The guard matters for
                // 0.49999999999999994, where `x + 0.5` rounds up to 1.0.
                let rounded = if value > 0.0 && value < 0.5 {
                    0.0
                } else {
                    (value + 0.5).floor()
                };
                Ok(Value::Number(if rounded == 0.0 && value < 0.0 {
                    -0.0
                } else {
                    rounded
                }))
            }
            "Math.trunc" => Ok(Value::Number(to_number(&argument(0)).trunc())),
            "Math.sqrt" => Ok(Value::Number(to_number(&argument(0)).sqrt())),
            "Math.sign" => Ok(Value::Number(math_sign(to_number(&argument(0))))),
            "Math.log" => Ok(Value::Number(to_number(&argument(0)).ln())),
            "Math.exp" => Ok(Value::Number(to_number(&argument(0)).exp())),
            "Math.sin" => Ok(Value::Number(to_number(&argument(0)).sin())),
            "Math.cos" => Ok(Value::Number(to_number(&argument(0)).cos())),
            "Math.tan" => Ok(Value::Number(to_number(&argument(0)).tan())),
            "Math.pow" => Ok(Value::Number(js_pow(
                to_number(&argument(0)),
                to_number(&argument(1)),
            ))),
            "Math.max" => Ok(Value::Number(math_extreme(arguments, true))),
            "Math.min" => Ok(Value::Number(math_extreme(arguments, false))),
            "JSON.parse" => {
                let Value::String(text) = argument(0) else {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::ExecutionFailure,
                        "JSON.parse expects a string.",
                        _span,
                    ));
                };
                let repaired = repair_overflow_numbers(&text);
                let parse_text = repaired.as_deref().unwrap_or(&text);
                let json: serde_json::Value =
                    serde_json::from_str(parse_text).map_err(|error| {
                        RuntimeError::new(
                            RuntimeErrorKind::SyntaxError,
                            format!("JSON.parse failed: {error}"),
                            _span,
                        )
                    })?;
                json_to_value_with_overflow(&json, _span, repaired.is_some())
            }
            "JSON.stringify" => {
                if let Some(Value::Array(_)) | Some(Value::Function(_)) = arguments.get(1) {
                    return Err(RuntimeError::unsupported("JSON.stringify replacer", _span));
                }
                let indent = match arguments.get(2) {
                    Some(Value::Number(number)) => {
                        let count = if number.is_nan() {
                            0
                        } else {
                            number.trunc().clamp(0.0, 10.0) as usize
                        };
                        " ".repeat(count)
                    }
                    Some(Value::String(text)) => text.chars().take(10).collect(),
                    _ => String::new(),
                };
                match stringify_json_with_indent(&argument(0), &indent) {
                    Ok(Some(text)) => Ok(Value::String(text)),
                    Ok(None) => Ok(Value::Undefined),
                    Err(JsonStringifyError::Circular) => Err(RuntimeError::type_error(
                        "Converting circular structure to JSON.",
                        _span,
                    )),
                    Err(JsonStringifyError::TooDeep) => Err(RuntimeError::type_error(
                        "Value is too deep to serialize as JSON.",
                        _span,
                    )),
                    Err(JsonStringifyError::NonData) => Err(RuntimeError::new(
                        RuntimeErrorKind::InvalidDataValue,
                        "JSON.stringify value must contain data only.",
                        _span,
                    )),
                    Err(JsonStringifyError::UnawaitedPromise) => Err(RuntimeError::new(
                        RuntimeErrorKind::InvalidDataValue,
                        "JSON.stringify cannot serialize an un-awaited Promise. \
                         Await it first, for example `await tools.ns.tool(...)`.",
                        _span,
                    )),
                }
            }
            "Promise.resolve" => {
                let value = argument(0);
                Ok(match value {
                    Value::Promise(_) => value,
                    _ => Value::promise_fulfilled(value),
                })
            }
            "Promise.reject" => Ok(Value::promise_rejected(argument(0))),
            "Promise.all" | "Promise.allSettled" | "Promise.race" => {
                let source = argument(0);
                let items = match &source {
                    Value::Array(items) => items.borrow().clone(),
                    _ => {
                        return Err(RuntimeError::type_error(
                            "Promise combinators require an array of values in CodeMode.",
                            _span,
                        ));
                    }
                };
                match name {
                    "Promise.race" => {
                        let Some(item) = items.into_iter().next() else {
                            return Err(RuntimeError::new(
                                RuntimeErrorKind::RangeError,
                                "Promise.race requires at least one item; an empty race would never settle.",
                                _span,
                            ));
                        };
                        Ok(match item {
                            Value::Promise(_) => item,
                            _ => Value::promise_fulfilled(item),
                        })
                    }
                    "Promise.all" => {
                        let mut values = Vec::new();
                        for item in &items {
                            match settle_promise(item) {
                                Ok(value) => values.push(value),
                                Err(reason) => return Ok(Value::promise_rejected(reason)),
                            }
                        }
                        Ok(Value::promise_fulfilled(Value::array(values)))
                    }
                    _ => {
                        let mut settled = Vec::new();
                        for item in &items {
                            let entry = match settle_promise(item) {
                                Ok(value) => Value::from_entries([
                                    ("status", Value::String("fulfilled".to_owned())),
                                    ("value", value),
                                ]),
                                Err(reason) => Value::from_entries([
                                    ("status", Value::String("rejected".to_owned())),
                                    ("reason", reason),
                                ]),
                            };
                            settled.push(entry);
                        }
                        Ok(Value::promise_fulfilled(Value::array(settled)))
                    }
                }
            }
            "Object.keys" => {
                let source = argument(0);
                if let Some(path) = source.tool_path() {
                    let keys = self
                        .tool_host
                        .as_ref()
                        .map(|host| host.keys(path))
                        .unwrap_or_default();
                    return Ok(Value::array(keys.into_iter().map(Value::String).collect()));
                }
                match &source {
                    Value::Object(_) | Value::Array(_) | Value::Error(_) => {}
                    Value::Date(_) | Value::Map(_) | Value::Set(_) | Value::Promise(_) => {
                        return Ok(Value::array(Vec::new()));
                    }
                    _ => {
                        return Err(RuntimeError::type_error(
                            "Object.keys expects a data object or array.",
                            _span,
                        ));
                    }
                }
                Ok(Value::array(
                    own_keys(&source).into_iter().map(Value::String).collect(),
                ))
            }
            "Object.values" => {
                let source = argument(0);
                if source.tool_path().is_some() {
                    return Err(RuntimeError::type_error(
                        "Object.values cannot read tool references. Use Object.keys(tools) to discover names.",
                        _span,
                    ));
                }
                match &source {
                    Value::Object(_) | Value::Error(_) => {}
                    Value::Date(_) | Value::Map(_) | Value::Set(_) | Value::Promise(_) => {
                        return Ok(Value::array(Vec::new()));
                    }
                    _ => {
                        return Err(RuntimeError::type_error(
                            "Object.values expects a data object.",
                            _span,
                        ));
                    }
                }
                let mut values = Vec::new();
                for key in own_keys(&source) {
                    values.push(self.get_property(&source, &key, _span)?);
                }
                Ok(Value::array(values))
            }
            "Object.entries" => {
                let source = argument(0);
                if source.tool_path().is_some() {
                    return Err(RuntimeError::type_error(
                        "Object.entries cannot read tool references. Use Object.keys(tools) to discover names.",
                        _span,
                    ));
                }
                match &source {
                    Value::Object(_) | Value::Error(_) => {}
                    Value::Date(_) | Value::Map(_) | Value::Set(_) | Value::Promise(_) => {
                        return Ok(Value::array(Vec::new()));
                    }
                    _ => {
                        return Err(RuntimeError::type_error(
                            "Object.entries expects a data object.",
                            _span,
                        ));
                    }
                }
                let mut entries = Vec::new();
                for key in own_keys(&source) {
                    let value = self.get_property(&source, &key, _span)?;
                    entries.push(Value::array(vec![Value::String(key), value]));
                }
                Ok(Value::array(entries))
            }
            "Object.hasOwn" => {
                let source = argument(0);
                if source.tool_path().is_some() {
                    return Err(RuntimeError::type_error(
                        "Object.hasOwn cannot read tool references. Use Object.keys(tools) to discover names.",
                        _span,
                    ));
                }
                let key = to_js_string(&argument(1));
                match &source {
                    Value::Object(object) => Ok(Value::Boolean(object.borrow().contains_key(&key))),
                    Value::Error(_) => Ok(Value::Boolean(own_keys(&source).contains(&key))),
                    Value::Date(_) | Value::Map(_) | Value::Set(_) | Value::Promise(_) => {
                        Ok(Value::Boolean(false))
                    }
                    _ => Err(RuntimeError::type_error(
                        "Object.hasOwn expects a data object.",
                        _span,
                    )),
                }
            }
            "Object.fromEntries" => {
                let source = argument(0);
                let items = match &source {
                    Value::Array(array) => array.borrow().clone(),
                    Value::Map(map) => map
                        .entries()
                        .into_iter()
                        .map(|(key, value)| Value::array(vec![key, value]))
                        .collect(),
                    _ => {
                        return Err(RuntimeError::type_error(
                            "Object.fromEntries requires an iterable of entries.",
                            _span,
                        ));
                    }
                };
                let mut object = IndexMap::new();
                for item in items {
                    let entry = match item {
                        Value::Array(entry) => entry.borrow().clone(),
                        _ => {
                            return Err(RuntimeError::type_error(
                                "Each Object.fromEntries item must be an entry array.",
                                _span,
                            ));
                        }
                    };
                    let key = entry.first().cloned().unwrap_or(Value::Undefined);
                    let key = to_js_string(&key);
                    if is_blocked_member(&key) {
                        return Err(RuntimeError::type_error(
                            format!("Property '{key}' is not available in CodeMode."),
                            _span,
                        ));
                    }
                    object.insert(key, entry.get(1).cloned().unwrap_or(Value::Undefined));
                }
                Ok(Value::Object(Rc::new(RefCell::new(object))))
            }
            "Object.assign" => {
                let mut merged = IndexMap::new();
                for source in arguments {
                    match source {
                        Value::Undefined | Value::Null => continue,
                        Value::Object(object) => {
                            let source = object.borrow();
                            for key in ordered_keys(&source) {
                                if is_blocked_member(&key) {
                                    return Err(RuntimeError::type_error(
                                        format!("Property '{key}' is not available in CodeMode."),
                                        _span,
                                    ));
                                }
                                let value = source.get(&key).expect("ordered keys exist").clone();
                                merged.insert(key, value);
                            }
                        }
                        Value::Error(error) => {
                            merged.insert(
                                "name".to_owned(),
                                Value::String(error.kind.name().to_owned()),
                            );
                            merged
                                .insert("message".to_owned(), Value::String(error.message.clone()));
                        }
                        Value::Date(_) | Value::Map(_) | Value::Set(_) => {}
                        Value::Promise(_) => {
                            return Err(RuntimeError::new(
                                RuntimeErrorKind::InvalidDataValue,
                                "Object.assign input contains an un-awaited Promise; \
                                 await it first.",
                                _span,
                            ));
                        }
                        _ => {
                            return Err(RuntimeError::type_error(
                                "Object.assign expects data objects.",
                                _span,
                            ));
                        }
                    }
                }
                Ok(Value::Object(Rc::new(RefCell::new(merged))))
            }
            "Array.isArray" => Ok(Value::Boolean(matches!(argument(0), Value::Array(_)))),
            "Array.of" => Ok(Value::array(arguments.to_vec())),
            "Array.from" => {
                if arguments.len() > 1 {
                    return Err(RuntimeError::unsupported("Array.from map function", _span));
                }
                let source = argument(0);
                match coercion_issue(&source, &mut Vec::new()) {
                    Some(CoercionIssue::Promise) => {
                        return Err(RuntimeError::new(
                            RuntimeErrorKind::InvalidDataValue,
                            "Array.from input contains an un-awaited Promise; await it first.",
                            _span,
                        ))
                    }
                    Some(CoercionIssue::NonData) => {
                        return Err(RuntimeError::new(
                            RuntimeErrorKind::InvalidDataValue,
                            "Array.from input must contain data only.",
                            _span,
                        ))
                    }
                    None => {}
                }
                // Like upstream, a value that is neither iterable nor
                // array-like yields an empty array instead of an error.
                let items = match &source {
                    Value::Array(_) | Value::String(_) | Value::Map(_) | Value::Set(_) => {
                        spread_items(&source).unwrap_or_default()
                    }
                    Value::Object(object) => {
                        // A numeric length is an array-like request; keep the
                        // allocation guard instead of silently returning [].
                        let oversized = match object.borrow().get("length") {
                            Some(Value::Number(length)) if length.is_finite() && *length >= 0.0 => {
                                length.trunc() > MAX_ARRAY_LIKE_LENGTH as f64
                            }
                            _ => false,
                        };
                        if oversized {
                            return Err(RuntimeError::type_error(
                                "Array.from array-like length exceeds the supported limit.",
                                _span,
                            ));
                        }
                        array_like_items(&source).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                Ok(Value::array(items))
            }
            "console.log" | "console.info" | "console.warn" | "console.error" | "console.debug" => {
                let prefix = match name {
                    "console.warn" => "[warn] ",
                    "console.error" => "[error] ",
                    "console.debug" => "[debug] ",
                    _ => "",
                };
                let message = arguments
                    .iter()
                    .map(format_console_argument)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.logs.push(format!("{prefix}{message}"));
                Ok(Value::Undefined)
            }
            "console.dir" => {
                self.logs.push(match arguments.first() {
                    Some(value) => format_console_argument(value),
                    None => "undefined".to_owned(),
                });
                Ok(Value::Undefined)
            }
            "console.table" => {
                self.logs
                    .push(format_console_table(arguments.first(), arguments.get(1)));
                Ok(Value::Undefined)
            }
            _ => Err(RuntimeError::unsupported(name, _span)),
        }
    }

    /// Implements the callable-replacement form of `String.replace` /
    /// `String.replaceAll` for plain-string searches. Offsets are UTF-16 code
    /// unit indices, matching JavaScript; strings containing lone surrogates
    /// cannot be represented, so matches never split a surrogate pair.
    fn replace_with_callback(
        &mut self,
        text: &str,
        search: &str,
        replacement: &Value,
        all: bool,
        span: Span,
    ) -> Result<String, RuntimeError> {
        let call = |this: &mut Self, matched: &str, offset: usize| {
            let result = this.invoke_value(
                replacement,
                vec![
                    Value::String(matched.to_owned()),
                    Value::Number(offset as f64),
                    Value::String(text.to_owned()),
                ],
                span,
            )?;
            if matches!(
                result,
                Value::Function(_)
                    | Value::Builtin(_)
                    | Value::BoundMethod { .. }
                    | Value::ToolRef(_)
            ) {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::InvalidDataValue,
                    "String.replace replacer result must be data.",
                    span,
                ));
            }
            Ok(to_js_string(&result))
        };

        let mut output = String::new();
        if search.is_empty() {
            let mut cursor = 0usize;
            let mut utf16 = 0usize;
            loop {
                output.push_str(&call(self, "", utf16)?);
                let Some(character) = text[cursor..].chars().next() else {
                    break;
                };
                output.push(character);
                cursor += character.len_utf8();
                utf16 += character.len_utf16();
                if !all {
                    output.push_str(&text[cursor..]);
                    break;
                }
            }
            return Ok(output);
        }

        let mut cursor = 0usize;
        while let Some(relative) = text[cursor..].find(search) {
            let start = cursor + relative;
            output.push_str(&text[cursor..start]);
            let offset = text[..start].encode_utf16().count();
            output.push_str(&call(self, search, offset)?);
            cursor = start + search.len();
            if !all {
                break;
            }
        }
        output.push_str(&text[cursor..]);
        Ok(output)
    }

    fn sort_items(
        &mut self,
        items: &mut [Value],
        comparator: Option<&Value>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if let Some(comparator) = comparator {
            if !comparator.is_undefined() && !matches!(comparator, Value::Function(_)) {
                return Err(RuntimeError::type_error(
                    "Array.sort expects an arrow function comparator.",
                    span,
                ));
            }
        }
        let undefined_count = items.iter().filter(|value| value.is_undefined()).count();
        if undefined_count > 0 {
            let mut defined = items
                .iter()
                .filter(|value| !value.is_undefined())
                .cloned()
                .collect::<Vec<_>>();
            self.sort_items(&mut defined, comparator, span)?;
            let tail = std::iter::repeat_with(|| Value::Undefined).take(undefined_count);
            for (slot, value) in items.iter_mut().zip(defined.into_iter().chain(tail)) {
                *slot = value;
            }
            return Ok(());
        }
        match comparator {
            Some(comparator) if !comparator.is_undefined() => {
                let mut index = 1;
                while index < items.len() {
                    let mut cursor = index;
                    while cursor > 0 {
                        let order = self.invoke_value(
                            comparator,
                            vec![items[cursor - 1].clone(), items[cursor].clone()],
                            span,
                        )?;
                        let order = to_number(&order);
                        if order > 0.0 {
                            items.swap(cursor - 1, cursor);
                            cursor -= 1;
                        } else {
                            break;
                        }
                    }
                    index += 1;
                }
            }
            _ => {
                let mut keyed = items
                    .iter()
                    .cloned()
                    .map(|item| {
                        let key = if item.is_undefined() {
                            None
                        } else {
                            Some(to_js_string(&item).encode_utf16().collect::<Vec<u16>>())
                        };
                        (key, item)
                    })
                    .collect::<Vec<_>>();
                keyed.sort_by(|(left, _), (right, _)| match (left, right) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(left), Some(right)) => left.cmp(right),
                });
                for (slot, (_, item)) in items.iter_mut().zip(keyed) {
                    *slot = item;
                }
            }
        }
        Ok(())
    }

    fn call_method(
        &mut self,
        method: &'static str,
        receiver: &Value,
        arguments: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let argument = |index: usize| arguments.get(index).cloned().unwrap_or(Value::Undefined);
        // Upstream's `optNumber`: missing/undefined is absent, but any other
        // non-number is rejected instead of being coerced. Index arguments are
        // therefore stricter than ordinary JavaScript arithmetic.
        let opt_number = |value: &Value, label: &str| -> Result<Option<f64>, RuntimeError> {
            match value {
                Value::Undefined => Ok(None),
                Value::Number(value) => Ok(Some(*value)),
                _ => Err(RuntimeError::type_error(
                    format!("{method} expects {label} to be a number."),
                    span,
                )),
            }
        };
        match method {
            "Array.push" => {
                let array = as_array_ref(receiver, span)?;
                let target = Rc::as_ptr(&array) as *const () as usize;
                for value in arguments {
                    if contains_identity(value, target, &mut Vec::new()) {
                        return Err(RuntimeError::new(
                            RuntimeErrorKind::InvalidDataValue,
                            "Array.push result contains a circular value.",
                            span,
                        ));
                    }
                }
                let mut array = array.borrow_mut();
                array.extend(arguments.iter().cloned());
                Ok(Value::Number(array.len() as f64))
            }
            "Array.pop" => {
                let array = as_array_ref(receiver, span)?;
                let value = array.borrow_mut().pop();
                Ok(value.unwrap_or(Value::Undefined))
            }
            "Array.shift" => {
                let array = as_array_ref(receiver, span)?;
                let mut array = array.borrow_mut();
                if array.is_empty() {
                    Ok(Value::Undefined)
                } else {
                    Ok(array.remove(0))
                }
            }
            "Array.unshift" => {
                let array = as_array_ref(receiver, span)?;
                let target = Rc::as_ptr(&array) as *const () as usize;
                for value in arguments {
                    if contains_identity(value, target, &mut Vec::new()) {
                        return Err(RuntimeError::new(
                            RuntimeErrorKind::InvalidDataValue,
                            "Array.unshift result contains a circular value.",
                            span,
                        ));
                    }
                }
                let mut array = array.borrow_mut();
                for (index, value) in arguments.iter().cloned().enumerate() {
                    array.insert(index, value);
                }
                Ok(Value::Number(array.len() as f64))
            }
            "Array.reverse" => {
                // Upstream returns a reversed copy: `reverse` and `toReversed` are
                // aliases so a bare `list.reverse()` never surprises later reads.
                let array = as_array_ref(receiver, span)?;
                let mut items = array.borrow().clone();
                items.reverse();
                Ok(Value::array(items))
            }
            "Array.map" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                // Upstream iterates a snapshot taken at call time and passes that
                // snapshot as the callback's third argument.
                let snapshot = Value::array(items.clone());
                let mapper = argument(0);
                let mut mapped = Vec::with_capacity(items.len());
                for (index, item) in items.into_iter().enumerate() {
                    mapped.push(self.invoke_value(
                        &mapper,
                        vec![item, Value::Number(index as f64), snapshot.clone()],
                        span,
                    )?);
                }
                Ok(Value::array(mapped))
            }
            "Array.filter" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let snapshot = Value::array(items.clone());
                let predicate = argument(0);
                let mut kept = Vec::new();
                for (index, item) in items.into_iter().enumerate() {
                    let result = self.invoke_value(
                        &predicate,
                        vec![item.clone(), Value::Number(index as f64), snapshot.clone()],
                        span,
                    )?;
                    if is_truthy(&result) {
                        kept.push(item);
                    }
                }
                Ok(Value::array(kept))
            }
            "Array.forEach" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let snapshot = Value::array(items.clone());
                let callback = argument(0);
                for (index, item) in items.into_iter().enumerate() {
                    self.invoke_value(
                        &callback,
                        vec![item, Value::Number(index as f64), snapshot.clone()],
                        span,
                    )?;
                }
                Ok(Value::Undefined)
            }
            "Array.reduce" | "Array.reduceRight" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let snapshot = Value::array(items.clone());
                let reducer = argument(0);
                let right = method == "Array.reduceRight";
                if right {
                    let (mut accumulator, start) = if arguments.len() > 1 {
                        (argument(1), items.len())
                    } else {
                        let Some(last) = items.last().cloned() else {
                            return Err(RuntimeError::type_error(
                                "Reduce of empty array with no initial value.",
                                span,
                            ));
                        };
                        (last, items.len().saturating_sub(1))
                    };
                    for index in (0..start).rev() {
                        accumulator = self.invoke_value(
                            &reducer,
                            vec![
                                accumulator,
                                items[index].clone(),
                                Value::Number(index as f64),
                                snapshot.clone(),
                            ],
                            span,
                        )?;
                    }
                    Ok(accumulator)
                } else {
                    let (mut accumulator, start) = if arguments.len() > 1 {
                        (argument(1), 0)
                    } else {
                        let Some(first) = items.first().cloned() else {
                            return Err(RuntimeError::type_error(
                                "Reduce of empty array with no initial value.",
                                span,
                            ));
                        };
                        (first, 1)
                    };
                    for (index, item) in items.into_iter().enumerate().skip(start) {
                        accumulator = self.invoke_value(
                            &reducer,
                            vec![
                                accumulator,
                                item,
                                Value::Number(index as f64),
                                snapshot.clone(),
                            ],
                            span,
                        )?;
                    }
                    Ok(accumulator)
                }
            }
            "Array.find"
            | "Array.findIndex"
            | "Array.findLast"
            | "Array.findLastIndex"
            | "Array.some"
            | "Array.every" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let snapshot = Value::array(items.clone());
                let predicate = argument(0);
                let reverse = matches!(method, "Array.findLast" | "Array.findLastIndex");
                let mut indexed = items.into_iter().enumerate().collect::<Vec<_>>();
                if reverse {
                    indexed.reverse();
                }
                let mut found = method == "Array.every";
                for (index, item) in indexed {
                    let result = self.invoke_value(
                        &predicate,
                        vec![item.clone(), Value::Number(index as f64), snapshot.clone()],
                        span,
                    )?;
                    let truthy = is_truthy(&result);
                    match method {
                        "Array.find" | "Array.findLast" if truthy => return Ok(item),
                        "Array.findIndex" | "Array.findLastIndex" if truthy => {
                            return Ok(Value::Number(index as f64));
                        }
                        "Array.some" if truthy => return Ok(Value::Boolean(true)),
                        "Array.every" if !truthy => {
                            found = false;
                            break;
                        }
                        _ => {}
                    }
                }
                Ok(match method {
                    "Array.find" | "Array.findLast" => Value::Undefined,
                    "Array.findIndex" | "Array.findLastIndex" => Value::Number(-1.0),
                    "Array.some" => Value::Boolean(false),
                    _ => Value::Boolean(found),
                })
            }
            "Array.join" => {
                if arguments.len() > 1
                    || (arguments.len() == 1 && !matches!(arguments[0], Value::String(_)))
                {
                    return Err(RuntimeError::type_error(
                        "Array.join expects zero arguments or one string separator.",
                        span,
                    ));
                }
                let array = as_array_ref(receiver, span)?;
                let separator = match arguments.first() {
                    Some(Value::String(separator)) => separator.clone(),
                    _ => ",".to_owned(),
                };
                let mut seen = vec![Rc::as_ptr(&array) as *const () as usize];
                let text = array
                    .borrow()
                    .iter()
                    .map(|item| {
                        if item.is_nullish() {
                            String::new()
                        } else {
                            to_js_string_seen(item, &mut seen)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Value::String(text))
            }
            "Array.slice" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let length = items.len() as i64;
                let start_number = opt_number(&argument(0), "start")?;
                let end_number = opt_number(&argument(1), "end")?;
                let start = start_number.map_or(0, |number| {
                    normalize_index(&Value::Number(number), length, 0)
                });
                let end = end_number.map_or(length, |number| {
                    normalize_index(&Value::Number(number), length, length)
                });
                let start = start.max(0) as usize;
                let end = end.max(0) as usize;
                let slice = if end > start && start < items.len() {
                    items[start..end.min(items.len())].to_vec()
                } else {
                    Vec::new()
                };
                Ok(Value::array(slice))
            }
            "Array.includes" | "Array.indexOf" => {
                if method == "Array.includes" && (arguments.is_empty() || arguments.len() > 2) {
                    return Err(RuntimeError::type_error(
                        "Array.includes expects a value and optional start index.",
                        span,
                    ));
                }
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let needle = argument(0);
                let from = opt_number(&argument(1), "start index")?
                    .map_or(0, |number| {
                        normalize_index(&Value::Number(number), items.len() as i64, 0)
                    })
                    .max(0) as usize;
                let position = items
                    .iter()
                    .enumerate()
                    .skip(from)
                    .find(|(_, item)| {
                        if method == "Array.includes" {
                            crate::value::same_value_zero(item, &needle)
                        } else {
                            strict_equals(item, &needle)
                        }
                    })
                    .map(|(index, _)| index);
                if method == "Array.includes" {
                    Ok(Value::Boolean(position.is_some()))
                } else {
                    Ok(Value::Number(position.map_or(-1.0, |index| index as f64)))
                }
            }
            "Array.lastIndexOf" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let needle = argument(0);
                let length = items.len() as i64;
                if length == 0 {
                    return Ok(Value::Number(-1.0));
                }
                let from = match arguments.get(1) {
                    // Upstream treats an explicit `undefined` like an omitted
                    // argument: search from the end.
                    Some(value) if !value.is_undefined() => {
                        let number = opt_number(value, "start index")?
                            .expect("non-undefined values are numbers");
                        if number.is_nan() {
                            0
                        } else {
                            let mut index = number.trunc() as i64;
                            if index < 0 {
                                index = index.saturating_add(length);
                            }
                            index
                        }
                    }
                    _ => length - 1,
                };
                if from < 0 {
                    return Ok(Value::Number(-1.0));
                }
                let start = from.min(length - 1) as usize;
                let position = items
                    .iter()
                    .enumerate()
                    .take(start + 1)
                    .rev()
                    .find(|(_, item)| strict_equals(item, &needle))
                    .map(|(index, _)| index);
                Ok(Value::Number(position.map_or(-1.0, |index| index as f64)))
            }
            "Array.concat" => {
                let array = as_array_ref(receiver, span)?;
                let mut values = array.borrow().clone();
                for argument in arguments {
                    match argument {
                        Value::Array(other) => values.extend(other.borrow().iter().cloned()),
                        value => values.push(value.clone()),
                    }
                }
                Ok(Value::array(values))
            }
            "Array.flat" => {
                let array = as_array_ref(receiver, span)?;
                let depth = opt_number(&argument(0), "depth")?.unwrap_or(1.0);
                let depth = depth.clamp(0.0, 64.0) as usize;
                let mut flattened = Vec::new();
                flatten_into(&array.borrow(), depth, &mut flattened);
                Ok(Value::array(flattened))
            }
            "Array.flatMap" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let snapshot = Value::array(items.clone());
                let mapper = argument(0);
                let mut mapped = Vec::with_capacity(items.len());
                for (index, item) in items.into_iter().enumerate() {
                    mapped.push(self.invoke_value(
                        &mapper,
                        vec![item, Value::Number(index as f64), snapshot.clone()],
                        span,
                    )?);
                }
                let mut flattened = Vec::new();
                flatten_into(&mapped, 1, &mut flattened);
                Ok(Value::array(flattened))
            }
            "Array.sort" => {
                // Like upstream, `sort` is non-mutating and equivalent to
                // `toSorted`; the receiver keeps its original order.
                let array = as_array_ref(receiver, span)?;
                let mut items = array.borrow().clone();
                self.sort_items(&mut items, arguments.first(), span)?;
                Ok(Value::array(items))
            }
            "Array.toSorted" => {
                let array = as_array_ref(receiver, span)?;
                let mut items = array.borrow().clone();
                self.sort_items(&mut items, arguments.first(), span)?;
                Ok(Value::array(items))
            }
            "Array.toReversed" => {
                let array = as_array_ref(receiver, span)?;
                let mut items = array.borrow().clone();
                items.reverse();
                Ok(Value::array(items))
            }
            "Array.with" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                let length = items.len() as i64;
                let index = opt_number(&argument(0), "index")?.unwrap_or(0.0);
                let resolved = if index < 0.0 {
                    length as f64 + index
                } else {
                    index
                };
                if resolved < 0.0 || resolved >= length as f64 {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::RangeError,
                        "Array.with index is out of range.",
                        span,
                    ));
                }
                let mut replaced = items;
                // Native `with` only writes a real element for integer indices;
                // fractional/NaN indices become non-index properties, which do
                // not appear in the returned array's contents.
                if resolved.fract() == 0.0 {
                    replaced[resolved as usize] = argument(1);
                }
                Ok(Value::array(replaced))
            }
            "Array.at" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow();
                let length = items.len() as i64;
                let index = opt_number(&argument(0), "index")?.unwrap_or(0.0);
                let index = normalize_index(&Value::Number(index), length, 0);
                Ok(if index < 0 || index >= length {
                    Value::Undefined
                } else {
                    items[index as usize].clone()
                })
            }
            "Array.splice" => {
                if arguments.is_empty() {
                    return Ok(Value::array(Vec::new()));
                }
                let array = as_array_ref(receiver, span)?;
                let length = array.borrow().len() as i64;
                let start_number = opt_number(&argument(0), "start")?.unwrap_or(0.0);
                let start = normalize_index(&Value::Number(start_number), length, 0)
                    .clamp(0, length) as usize;
                let delete_count = if arguments.len() > 1 {
                    let count = opt_number(&argument(1), "delete count")?.unwrap_or(0.0);
                    if count.is_nan() {
                        0
                    } else {
                        count.max(0.0).min((length - start as i64) as f64) as usize
                    }
                } else {
                    (length as usize) - start
                };
                let inserted = arguments.iter().skip(2).cloned().collect::<Vec<_>>();
                let target = Rc::as_ptr(&array) as *const () as usize;
                if inserted
                    .iter()
                    .any(|value| contains_identity(value, target, &mut Vec::new()))
                {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::InvalidDataValue,
                        "Array.splice result would be circular because it contains the receiver.",
                        span,
                    ));
                }
                let removed = {
                    let mut array = array.borrow_mut();
                    let removed = array.drain(start..start + delete_count).collect::<Vec<_>>();
                    for (offset, value) in inserted.into_iter().enumerate() {
                        array.insert(start + offset, value);
                    }
                    removed
                };
                Ok(Value::array(removed))
            }
            "Array.fill" => {
                let array = as_array_ref(receiver, span)?;
                let length = array.borrow().len() as i64;
                let value = argument(0);
                let target = Rc::as_ptr(&array) as *const () as usize;
                if contains_identity(&value, target, &mut Vec::new()) {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::InvalidDataValue,
                        "Array.fill result contains a circular value.",
                        span,
                    ));
                }
                let start_number = opt_number(&argument(1), "start")?;
                let end_number = opt_number(&argument(2), "end")?;
                let start = start_number.map_or(0, |number| {
                    normalize_index(&Value::Number(number), length, 0)
                });
                let end = end_number.map_or(length, |number| {
                    normalize_index(&Value::Number(number), length, length)
                });
                let start = start.clamp(0, length) as usize;
                let end = end.clamp(0, length) as usize;
                let mut items = array.borrow_mut();
                for item in items.iter_mut().take(end).skip(start) {
                    *item = value.clone();
                }
                drop(items);
                Ok(receiver.clone())
            }
            "Array.copyWithin" => {
                let array = as_array_ref(receiver, span)?;
                let length = array.borrow().len() as i64;
                let target_number = opt_number(&argument(0), "target index")?.unwrap_or(0.0);
                let start_number = opt_number(&argument(1), "start")?.unwrap_or(0.0);
                let end_number = opt_number(&argument(2), "end")?;
                let target = normalize_index(&Value::Number(target_number), length, 0)
                    .clamp(0, length) as usize;
                let start = normalize_index(&Value::Number(start_number), length, 0)
                    .clamp(0, length) as usize;
                let end = end_number.map_or(length, |number| {
                    normalize_index(&Value::Number(number), length, length)
                });
                let end = end.clamp(0, length) as usize;
                let count = end.saturating_sub(start).min(length as usize - target);
                let copied = {
                    let items = array.borrow();
                    items[start..start + count].to_vec()
                };
                let mut items = array.borrow_mut();
                items[target..target + count].clone_from_slice(&copied);
                drop(items);
                Ok(receiver.clone())
            }
            "Array.keys" => {
                let array = as_array_ref(receiver, span)?;
                let length = array.borrow().len();
                Ok(Value::array(
                    (0..length)
                        .map(|index| Value::Number(index as f64))
                        .collect(),
                ))
            }
            "Array.values" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                Ok(Value::array(items))
            }
            "Array.entries" => {
                let array = as_array_ref(receiver, span)?;
                let items = array.borrow().clone();
                Ok(Value::array(
                    items
                        .into_iter()
                        .enumerate()
                        .map(|(index, value)| {
                            Value::array(vec![Value::Number(index as f64), value])
                        })
                        .collect(),
                ))
            }
            "String.split" => {
                let text = as_string(receiver, span)?;
                // ECMAScript applies ToUint32 to the limit: NaN and Infinity
                // become 0, and negative values wrap around.
                let limit = match arguments.get(1) {
                    Some(Value::Undefined) | None => u32::MAX as usize,
                    Some(value) => to_uint32(to_number(value)) as usize,
                };
                let pieces: Vec<String> = match arguments.first() {
                    None | Some(Value::Undefined) => vec![text],
                    Some(Value::String(separator)) if separator.is_empty() => text
                        .chars()
                        .map(|character| character.to_string())
                        .collect(),
                    Some(separator) => {
                        let separator = to_js_string(separator);
                        text.split(&separator).map(str::to_owned).collect()
                    }
                };
                Ok(Value::array(
                    pieces.into_iter().take(limit).map(Value::String).collect(),
                ))
            }
            "String.slice" | "String.substring" | "String.substr" => {
                let text = as_string(receiver, span)?;
                let characters: Vec<char> = text.chars().collect();
                let length = characters.len() as i64;
                let start_argument = argument(0);
                let end_argument = argument(1);
                let (start, end) = match method {
                    "String.substring" => {
                        let start = clamp_index(to_number(&start_argument), length);
                        let end = if end_argument.is_undefined() {
                            length
                        } else {
                            clamp_index(to_number(&end_argument), length)
                        };
                        if start <= end {
                            (start, end)
                        } else {
                            (end, start)
                        }
                    }
                    "String.substr" => {
                        let start = normalize_index(&start_argument, length, 0).max(0);
                        let count = if end_argument.is_undefined() {
                            length
                        } else {
                            to_number(&end_argument).max(0.0) as i64
                        };
                        (start, start.saturating_add(count))
                    }
                    _ => (
                        normalize_index(&start_argument, length, 0),
                        normalize_index(&end_argument, length, length),
                    ),
                };
                let start = start.max(0).min(length) as usize;
                let end = end.max(0).min(length) as usize;
                let slice = if end > start {
                    characters[start..end].iter().collect()
                } else {
                    String::new()
                };
                Ok(Value::String(slice))
            }
            "String.indexOf" | "String.lastIndexOf" => {
                let text = as_string(receiver, span)?;
                let needle: Vec<char> = to_js_string(&argument(0)).chars().collect();
                let characters: Vec<char> = text.chars().collect();
                let length = characters.len() as i64;
                let from = if method == "String.lastIndexOf" {
                    // lastIndexOf takes an absolute position: negatives clamp to
                    // 0 and NaN defaults to the end of the string.
                    match arguments.get(1) {
                        None | Some(Value::Undefined) => length,
                        Some(value) => {
                            let number = to_number(value);
                            if number.is_nan() {
                                length
                            } else {
                                (number.trunc() as i64).clamp(0, length)
                            }
                        }
                    }
                } else {
                    absolute_index(&argument(1), length, 0)
                } as usize;
                let matches = (0..=characters.len().saturating_sub(needle.len()))
                    .filter(|index| characters[*index..].starts_with(&needle))
                    .collect::<Vec<_>>();
                let position = if method == "String.indexOf" {
                    matches.into_iter().find(|index| *index >= from)
                } else {
                    matches.into_iter().rfind(|index| *index <= from)
                };
                Ok(Value::Number(position.map_or(-1.0, |index| index as f64)))
            }
            "String.includes" | "String.startsWith" | "String.endsWith" => {
                let text = as_string(receiver, span)?;
                let needle: Vec<char> = to_js_string(&argument(0)).chars().collect();
                let characters: Vec<char> = text.chars().collect();
                let length = characters.len();
                let default = if method == "String.endsWith" {
                    length as i64
                } else {
                    0
                };
                let position = absolute_index(&argument(1), length as i64, default) as usize;
                let result = match method {
                    "String.endsWith" => {
                        position >= needle.len()
                            && characters[position - needle.len()..position] == needle[..]
                    }
                    "String.startsWith" => {
                        position + needle.len() <= length
                            && characters[position..position + needle.len()] == needle[..]
                    }
                    _ => {
                        needle.is_empty()
                            || (position <= length
                                && (position..=length.saturating_sub(needle.len()))
                                    .any(|index| characters[index..].starts_with(&needle)))
                    }
                };
                Ok(Value::Boolean(result))
            }
            "String.trim" => Ok(Value::String(
                as_string(receiver, span)?
                    .trim_matches(is_js_whitespace)
                    .to_owned(),
            )),
            "String.trimStart" => Ok(Value::String(
                as_string(receiver, span)?
                    .trim_start_matches(is_js_whitespace)
                    .to_owned(),
            )),
            "String.trimEnd" => Ok(Value::String(
                as_string(receiver, span)?
                    .trim_end_matches(is_js_whitespace)
                    .to_owned(),
            )),
            "String.toUpperCase" => Ok(Value::String(as_string(receiver, span)?.to_uppercase())),
            "String.toLowerCase" => Ok(Value::String(as_string(receiver, span)?.to_lowercase())),
            "String.localeCompare" => {
                let text = as_string(receiver, span)?;
                let other = to_js_string(&argument(0));
                Ok(Value::Number(match text.cmp(&other) {
                    std::cmp::Ordering::Less => -1.0,
                    std::cmp::Ordering::Equal => 0.0,
                    std::cmp::Ordering::Greater => 1.0,
                }))
            }
            "String.charAt" => {
                let text = as_string(receiver, span)?;
                let character =
                    string_index(&argument(0)).and_then(|index| text.chars().nth(index));
                Ok(Value::String(
                    character.map_or_else(String::new, |character| character.to_string()),
                ))
            }
            "String.charCodeAt" | "String.codePointAt" => {
                let text = as_string(receiver, span)?;
                let code = string_index(&argument(0))
                    .and_then(|index| text.chars().nth(index))
                    .map(u32::from);
                Ok(match (method, code) {
                    ("String.charCodeAt", code) => Value::Number(code.map_or(f64::NAN, f64::from)),
                    (_, Some(code)) => Value::Number(f64::from(code)),
                    (_, None) => Value::Undefined,
                })
            }
            "String.at" => {
                let text = as_string(receiver, span)?;
                let characters: Vec<char> = text.chars().collect();
                let length = characters.len() as i64;
                let index = normalize_index(&argument(0), length, 0);
                Ok(if index < 0 || index >= length {
                    Value::Undefined
                } else {
                    Value::String(characters[index as usize].to_string())
                })
            }
            "String.padStart" | "String.padEnd" => {
                let text = as_string(receiver, span)?;
                let number = to_number(&argument(0));
                if number == f64::INFINITY || number > MAX_STRING_LENGTH as f64 {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::RangeError,
                        "Invalid string length.",
                        span,
                    ));
                }
                let target = if number.is_nan() {
                    0
                } else {
                    number.max(0.0).trunc() as usize
                };
                let padding = match arguments.get(1) {
                    Some(Value::Undefined) | None => " ".to_owned(),
                    Some(value) => to_js_string(value),
                };
                Ok(Value::String(pad_string(
                    &text,
                    target,
                    &padding,
                    method == "String.padStart",
                )))
            }
            "String.repeat" => {
                let text = as_string(receiver, span)?;
                let number = to_number(&argument(0));
                let count = if number.is_nan() { 0.0 } else { number.trunc() };
                if count < 0.0 || count == f64::INFINITY {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::RangeError,
                        "Invalid count value.",
                        span,
                    ));
                }
                let count = count as usize;
                if text.len().saturating_mul(count) > MAX_STRING_LENGTH {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::RangeError,
                        "Invalid string length.",
                        span,
                    ));
                }
                Ok(Value::String(text.repeat(count)))
            }
            "String.replace" | "String.replaceAll" => {
                let text = as_string(receiver, span)?;
                let Value::String(search) = argument(0) else {
                    return Err(RuntimeError::type_error(
                        format!("{method} expects argument 1 to be a string."),
                        span,
                    ));
                };
                let replacement = argument(1);
                let all = method == "String.replaceAll";
                if matches!(
                    replacement,
                    Value::Function(_) | Value::Builtin(_) | Value::BoundMethod { .. }
                ) {
                    return Ok(Value::String(self.replace_with_callback(
                        &text,
                        &search,
                        &replacement,
                        all,
                        span,
                    )?));
                }
                Ok(Value::String(replace_string(
                    &text,
                    &search,
                    &to_js_string(&replacement),
                    all,
                )))
            }
            "String.toString" => Ok(Value::String(as_string(receiver, span)?)),
            "String.concat" => {
                let mut text = as_string(receiver, span)?;
                for argument in arguments {
                    text.push_str(&to_js_string(argument));
                }
                Ok(Value::String(text))
            }
            "Number.toFixed" => {
                let number = as_number(receiver, span)?;
                let digits =
                    optional_digits(arguments.first(), "toFixed", 0, 100, span)?.unwrap_or(0);
                if number.abs() >= 1e21 {
                    return Ok(Value::String(format_number(number)));
                }
                let text = round_decimal(number.abs(), digits as i32);
                Ok(Value::String(if number < 0.0 {
                    format!("-{text}")
                } else {
                    text
                }))
            }
            "Number.toPrecision" => {
                let number = as_number(receiver, span)?;
                match optional_digits(arguments.first(), "toPrecision", 1, 100, span)? {
                    None => Ok(Value::String(format_number(number))),
                    Some(precision) => Ok(Value::String(number_to_precision(number, precision))),
                }
            }
            "Number.toExponential" => {
                let number = as_number(receiver, span)?;
                let digits = optional_digits(arguments.first(), "toExponential", 0, 100, span)?;
                Ok(Value::String(number_to_exponential(number, digits)))
            }
            "Number.toString" => {
                let number = as_number(receiver, span)?;
                let radix = match arguments.first() {
                    None | Some(Value::Undefined) => 10,
                    Some(value) => {
                        let number = to_number(value);
                        let radix = if number.is_nan() { 0.0 } else { number.trunc() };
                        if !(2.0..=36.0).contains(&radix) {
                            return Err(RuntimeError::new(
                                RuntimeErrorKind::RangeError,
                                "toString() radix must be between 2 and 36.",
                                span,
                            ));
                        }
                        radix as u32
                    }
                };
                Ok(Value::String(number_to_string(number, radix)))
            }
            "Number.valueOf" => Ok(Value::Number(as_number(receiver, span)?)),
            "Date.getTime" | "Date.valueOf" => Ok(Value::Number(as_date_time(receiver, span)?)),
            "Date.toISOString" => crate::date::iso_string(as_date_time(receiver, span)?)
                .map(Value::String)
                .ok_or_else(|| {
                    RuntimeError::new(RuntimeErrorKind::RangeError, "Invalid time value.", span)
                }),
            "Date.toJSON" => Ok(crate::date::iso_string(as_date_time(receiver, span)?)
                .map_or(Value::Null, Value::String)),
            "Date.toString" => Ok(Value::String(to_js_string(receiver))),
            "Date.getFullYear" => Ok(date_component(receiver, span, |parts| parts.year as f64)),
            "Date.getMonth" => Ok(date_component(receiver, span, |parts| parts.month as f64)),
            "Date.getDate" => Ok(date_component(receiver, span, |parts| parts.day as f64)),
            "Date.getDay" => Ok(date_component(receiver, span, |parts| parts.weekday as f64)),
            "Date.getHours" => Ok(date_component(receiver, span, |parts| parts.hours as f64)),
            "Date.getMinutes" => Ok(date_component(receiver, span, |parts| parts.minutes as f64)),
            "Date.getSeconds" => Ok(date_component(receiver, span, |parts| parts.seconds as f64)),
            "Date.getMilliseconds" => Ok(date_component(receiver, span, |parts| {
                parts.milliseconds as f64
            })),
            "Date.getTimezoneOffset" => {
                let time_ms = as_date_time(receiver, span)?;
                Ok(Value::Number(if crate::date::parts(time_ms).is_some() {
                    0.0
                } else {
                    f64::NAN
                }))
            }
            "Map.get" => {
                let map = as_map_ref(receiver, span)?;
                Ok(map.get(&argument(0)).unwrap_or(Value::Undefined))
            }
            "Map.has" => {
                let map = as_map_ref(receiver, span)?;
                Ok(Value::Boolean(map.get(&argument(0)).is_some()))
            }
            "Map.set" => {
                let map = as_map_ref(receiver, span)?;
                map.insert(argument(0), argument(1));
                Ok(receiver.clone())
            }
            "Map.delete" => {
                let map = as_map_ref(receiver, span)?;
                Ok(Value::Boolean(map.remove(&argument(0)).is_some()))
            }
            "Map.clear" => {
                let map = as_map_ref(receiver, span)?;
                map.clear();
                Ok(Value::Undefined)
            }
            "Map.keys" => {
                let map = as_map_ref(receiver, span)?;
                Ok(Value::array(map.keys()))
            }
            "Map.values" => {
                let map = as_map_ref(receiver, span)?;
                Ok(Value::array(map.values()))
            }
            "Map.entries" => {
                let map = as_map_ref(receiver, span)?;
                Ok(Value::array(
                    map.entries()
                        .into_iter()
                        .map(|(key, value)| Value::array(vec![key, value]))
                        .collect(),
                ))
            }
            "Map.forEach" => {
                let map = as_map_ref(receiver, span)?;
                let callback = argument(0);
                for (key, value) in map.entries() {
                    self.invoke_value(&callback, vec![value, key, receiver.clone()], span)?;
                }
                Ok(Value::Undefined)
            }
            "Set.has" => {
                let set = as_set_ref(receiver, span)?;
                Ok(Value::Boolean(set.has(&argument(0))))
            }
            "Set.add" => {
                let set = as_set_ref(receiver, span)?;
                set.add(argument(0));
                Ok(receiver.clone())
            }
            "Set.delete" => {
                let set = as_set_ref(receiver, span)?;
                Ok(Value::Boolean(set.remove(&argument(0))))
            }
            "Set.clear" => {
                let set = as_set_ref(receiver, span)?;
                set.clear();
                Ok(Value::Undefined)
            }
            "Set.keys" | "Set.values" => {
                let set = as_set_ref(receiver, span)?;
                Ok(Value::array(set.values()))
            }
            "Set.entries" => {
                let set = as_set_ref(receiver, span)?;
                Ok(Value::array(
                    set.values()
                        .into_iter()
                        .map(|value| Value::array(vec![value.clone(), value]))
                        .collect(),
                ))
            }
            "Set.forEach" => {
                let set = as_set_ref(receiver, span)?;
                let callback = argument(0);
                for value in set.values() {
                    self.invoke_value(
                        &callback,
                        vec![value.clone(), value, receiver.clone()],
                        span,
                    )?;
                }
                Ok(Value::Undefined)
            }
            _ => Err(RuntimeError::unsupported(method, span)),
        }
    }

    fn invoke_tool(
        &mut self,
        path: &Rc<Vec<String>>,
        arguments: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if path.is_empty() {
            return Err(RuntimeError::type_error(
                "The tools root is not callable.",
                span,
            ));
        }
        let name = path.join(".");
        self.tool_budget.admit(name.clone()).map_err(|error| {
            RuntimeError::new(
                RuntimeErrorKind::ToolCallLimitExceeded,
                error.to_string(),
                span,
            )
        })?;
        for (index, argument) in arguments.iter().enumerate() {
            check_data(argument, &format!("tool argument {}", index + 1)).map_err(|error| {
                RuntimeError::new(RuntimeErrorKind::InvalidDataValue, error.to_string(), span)
            })?;
        }
        let Some(host) = self.tool_host.as_mut() else {
            let message = format!("Tool '{name}' is not available in this execution.");
            let error = ToolError::new(message.clone());
            return Err(RuntimeError::tool_failure(
                message,
                tool_error_value(&error, span),
                span,
            ));
        };
        let arguments = arguments.iter().map(Value::to_json).collect();
        match host.invoke(path.as_slice(), arguments) {
            Ok(json) => {
                let value = json_to_value(&json, span)?;
                check_data(&value, &format!("tool '{name}' result")).map_err(|error| {
                    RuntimeError::new(RuntimeErrorKind::InvalidDataValue, error.to_string(), span)
                })?;
                Ok(value)
            }
            Err(error) => Err(RuntimeError::tool_failure(
                error.message().to_owned(),
                tool_error_value(&error, span),
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
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(
                RuntimeErrorKind::RangeError,
                format!("Maximum call depth of {MAX_CALL_DEPTH} exceeded."),
                span,
            ));
        }
        self.call_depth += 1;
        let result = self.invoke_function_inner(id, arguments, span);
        self.call_depth -= 1;
        result
    }

    fn invoke_function_inner(
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
                    Err(_) => Ok(bound_method(object, array_method(key))),
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
                    Err(_) => Ok(bound_method(object, string_method(key))),
                }
            }
            Value::Builtin(name) => Ok(builtin_property(name, key)),
            Value::Error(error) => Ok(match key {
                "name" => Value::String(error.kind.name().to_owned()),
                "message" => Value::String(error.message.clone()),
                _ => Value::Undefined,
            }),
            Value::Date(_) => Ok(bound_method(object, date_method(key))),
            Value::Map(map) => {
                if key == "size" {
                    return Ok(Value::Number(map.len() as f64));
                }
                Ok(bound_method(object, map_method(key)))
            }
            Value::Set(set) => {
                if key == "size" {
                    return Ok(Value::Number(set.len() as f64));
                }
                Ok(bound_method(object, set_method(key)))
            }
            Value::Promise(_) => match key {
                "then" | "catch" | "finally" => Err(RuntimeError::new(
                    RuntimeErrorKind::UnsupportedSyntax,
                    format!(
                        "Promise.prototype.{key} is not supported in CodeMode. Use await instead."
                    ),
                    span,
                )),
                _ => Err(RuntimeError::new(
                    RuntimeErrorKind::InvalidDataValue,
                    format!("Cannot read '{key}' from an un-awaited Promise; await it first."),
                    span,
                )),
            },
            Value::Number(_) => Ok(bound_method(object, number_method(key))),
            Value::ToolRef(path) => {
                let mut next = path.as_ref().clone();
                next.push(key.to_owned());
                Ok(Value::tool_ref(next))
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

fn settle_promise(value: &Value) -> Result<Value, Value> {
    match value {
        Value::Promise(promise) => match &**promise {
            PromiseState::Fulfilled(value) => Ok(value.clone()),
            PromiseState::Rejected(reason) => Err(reason.clone()),
        },
        other => Ok(other.clone()),
    }
}

fn construct_map(init: Option<&Value>, span: Span) -> Result<Value, RuntimeError> {
    let map = MapValue::new();
    match init {
        None | Some(Value::Undefined) | Some(Value::Null) => {}
        Some(Value::Array(entries)) => {
            for pair in entries.borrow().iter() {
                let Value::Array(pair) = pair else {
                    return Err(RuntimeError::type_error(
                        "new Map(...) expects [key, value] pairs.",
                        span,
                    ));
                };
                let pair = pair.borrow();
                map.insert(
                    pair.first().cloned().unwrap_or(Value::Undefined),
                    pair.get(1).cloned().unwrap_or(Value::Undefined),
                );
            }
        }
        Some(Value::Map(source)) => {
            for (key, value) in source.entries() {
                map.insert(key, value);
            }
        }
        Some(_) => {
            return Err(RuntimeError::type_error(
                "new Map(...) expects an array of [key, value] pairs, a Map, or no argument.",
                span,
            ));
        }
    }
    Ok(Value::Map(Rc::new(map)))
}

fn construct_set(init: Option<&Value>, span: Span) -> Result<Value, RuntimeError> {
    let set = SetValue::new();
    match init {
        None | Some(Value::Undefined) | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items.borrow().iter() {
                set.add(item.clone());
            }
        }
        Some(Value::Set(source)) => {
            for value in source.values() {
                set.add(value);
            }
        }
        Some(Value::String(text)) => {
            for character in text.chars() {
                set.add(Value::String(character.to_string()));
            }
        }
        Some(_) => {
            return Err(RuntimeError::type_error(
                "new Set(...) expects an array, Set, string, or no argument.",
                span,
            ));
        }
    }
    Ok(Value::Set(Rc::new(set)))
}

fn construct_date(arguments: &[Value]) -> Value {
    let time_ms = match arguments {
        [] => crate::date::now_ms(),
        [value] => match value {
            Value::Date(date) => date.time_ms,
            Value::Number(number) => *number,
            Value::String(text) => crate::date::parse_iso(text),
            _ => f64::NAN,
        },
        values => {
            let values = values.iter().map(to_number).collect::<Vec<_>>();
            crate::date::from_components(&values)
        }
    };
    Value::date(time_ms)
}

fn make_error(name: &str, arguments: &[Value]) -> Value {
    let kind = ErrorKind::from_name(name).unwrap_or(ErrorKind::Error);
    let message = match arguments.first() {
        None | Some(Value::Undefined) => String::new(),
        Some(value) => to_js_string(value),
    };
    Value::error(kind, message)
}

fn instance_of(left: &Value, right: &Value, span: Span) -> Result<bool, RuntimeError> {
    let Value::Builtin(name) = right else {
        return Err(RuntimeError::type_error(
            "The right-hand side of 'instanceof' must be a constructor CodeMode knows: \
             Error (or a specific error type like TypeError), Date, RegExp, Map, Set, URL, \
             URLSearchParams, Array, Object, or Promise.",
            span,
        ));
    };
    if let Some(kind) = ErrorKind::from_name(name) {
        return Ok(match left {
            Value::Error(error) => kind == ErrorKind::Error || error.kind == kind,
            _ => false,
        });
    }
    Ok(match *name {
        "Map" => matches!(left, Value::Map(_)),
        "Set" => matches!(left, Value::Set(_)),
        "Date" => matches!(left, Value::Date(_)),
        "Array" => matches!(left, Value::Array(_)),
        // `Object` matches every value that JS calls an object or a function;
        // CodeMode namespaces and callables count as functions.
        "Object" => matches!(
            left,
            Value::Array(_)
                | Value::Object(_)
                | Value::Error(_)
                | Value::Promise(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::Date(_)
                | Value::Function(_)
                | Value::Builtin(_)
                | Value::BoundMethod { .. }
                | Value::ToolRef(_)
        ),
        // Number/String/Boolean wrap primitives in JS; no boxed values exist in
        // CodeMode, so primitives never match these constructors.
        "Number" | "String" | "Boolean" => false,
        "Promise" => matches!(left, Value::Promise(_)),
        _ => {
            return Err(RuntimeError::type_error(
                "The right-hand side of 'instanceof' must be a constructor CodeMode knows: \
                 Error (or a specific error type like TypeError), Date, RegExp, Map, Set, URL, \
                 URLSearchParams, Array, Object, or Promise.",
                span,
            ))
        }
    })
}

fn builtin_property(name: &str, key: &str) -> Value {
    match (name, key) {
        ("Number", "isNaN") => Value::Builtin("Number.isNaN"),
        ("Number", "isFinite") => Value::Builtin("Number.isFinite"),
        ("Number", "isInteger") => Value::Builtin("Number.isInteger"),
        ("Number", "isSafeInteger") => Value::Builtin("Number.isSafeInteger"),
        ("Number", "parseInt") => Value::Builtin("Number.parseInt"),
        ("Number", "parseFloat") => Value::Builtin("Number.parseFloat"),
        ("Number", "MAX_SAFE_INTEGER") => Value::Number(9_007_199_254_740_991.0),
        ("Number", "MIN_SAFE_INTEGER") => Value::Number(-9_007_199_254_740_991.0),
        ("Number", "EPSILON") => Value::Number(f64::EPSILON),
        ("Number", "MAX_VALUE") => Value::Number(f64::MAX),
        ("Number", "MIN_VALUE") => Value::Number(f64::from_bits(1)),
        ("Number", "POSITIVE_INFINITY") => Value::Number(f64::INFINITY),
        ("Number", "NEGATIVE_INFINITY") => Value::Number(f64::NEG_INFINITY),
        ("Number", "NaN") => Value::Number(f64::NAN),
        ("String", "fromCharCode") => Value::Builtin("String.fromCharCode"),
        ("String", "fromCodePoint") => Value::Builtin("String.fromCodePoint"),
        ("Object", "keys") => Value::Builtin("Object.keys"),
        ("Object", "values") => Value::Builtin("Object.values"),
        ("Object", "entries") => Value::Builtin("Object.entries"),
        ("Object", "fromEntries") => Value::Builtin("Object.fromEntries"),
        ("Object", "hasOwn") => Value::Builtin("Object.hasOwn"),
        ("Object", "assign") => Value::Builtin("Object.assign"),
        ("Array", "isArray") => Value::Builtin("Array.isArray"),
        ("Array", "from") => Value::Builtin("Array.from"),
        ("Array", "of") => Value::Builtin("Array.of"),
        ("Promise", "resolve") => Value::Builtin("Promise.resolve"),
        ("Promise", "reject") => Value::Builtin("Promise.reject"),
        ("Promise", "all") => Value::Builtin("Promise.all"),
        ("Promise", "allSettled") => Value::Builtin("Promise.allSettled"),
        ("Promise", "race") => Value::Builtin("Promise.race"),
        ("Date", "now") => Value::Builtin("Date.now"),
        ("Date", "parse") => Value::Builtin("Date.parse"),
        ("Date", "UTC") => Value::Builtin("Date.UTC"),
        _ => Value::Undefined,
    }
}

fn bound_method(receiver: &Value, method: Option<&'static str>) -> Value {
    match method {
        Some(method) => Value::BoundMethod {
            receiver: Box::new(receiver.clone()),
            method,
        },
        None => Value::Undefined,
    }
}

fn array_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "map" => "Array.map",
        "filter" => "Array.filter",
        "forEach" => "Array.forEach",
        "reduce" => "Array.reduce",
        "reduceRight" => "Array.reduceRight",
        "find" => "Array.find",
        "findIndex" => "Array.findIndex",
        "findLast" => "Array.findLast",
        "findLastIndex" => "Array.findLastIndex",
        "some" => "Array.some",
        "every" => "Array.every",
        "join" => "Array.join",
        "slice" => "Array.slice",
        "includes" => "Array.includes",
        "indexOf" => "Array.indexOf",
        "lastIndexOf" => "Array.lastIndexOf",
        "push" => "Array.push",
        "pop" => "Array.pop",
        "shift" => "Array.shift",
        "unshift" => "Array.unshift",
        "reverse" => "Array.reverse",
        "concat" => "Array.concat",
        "flat" => "Array.flat",
        "flatMap" => "Array.flatMap",
        "sort" => "Array.sort",
        "toSorted" => "Array.toSorted",
        "toReversed" => "Array.toReversed",
        "with" => "Array.with",
        "at" => "Array.at",
        "splice" => "Array.splice",
        "fill" => "Array.fill",
        "copyWithin" => "Array.copyWithin",
        "keys" => "Array.keys",
        "values" => "Array.values",
        "entries" => "Array.entries",
        _ => return None,
    })
}

fn string_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "split" => "String.split",
        "slice" => "String.slice",
        "substring" => "String.substring",
        "substr" => "String.substr",
        "indexOf" => "String.indexOf",
        "lastIndexOf" => "String.lastIndexOf",
        "includes" => "String.includes",
        "startsWith" => "String.startsWith",
        "endsWith" => "String.endsWith",
        "trim" => "String.trim",
        "trimStart" => "String.trimStart",
        "trimEnd" => "String.trimEnd",
        "trimLeft" => "String.trimStart",
        "trimRight" => "String.trimEnd",
        "localeCompare" => "String.localeCompare",
        "toUpperCase" => "String.toUpperCase",
        "toLowerCase" => "String.toLowerCase",
        "charAt" => "String.charAt",
        "charCodeAt" => "String.charCodeAt",
        "at" => "String.at",
        "padStart" => "String.padStart",
        "padEnd" => "String.padEnd",
        "repeat" => "String.repeat",
        "replace" => "String.replace",
        "replaceAll" => "String.replaceAll",
        "codePointAt" => "String.codePointAt",
        "toString" => "String.toString",
        "concat" => "String.concat",
        _ => return None,
    })
}

fn number_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "toFixed" => "Number.toFixed",
        "toPrecision" => "Number.toPrecision",
        "toExponential" => "Number.toExponential",
        "toString" => "Number.toString",
        "valueOf" => "Number.valueOf",
        _ => return None,
    })
}

fn map_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "get" => "Map.get",
        "set" => "Map.set",
        "has" => "Map.has",
        "delete" => "Map.delete",
        "clear" => "Map.clear",
        "forEach" => "Map.forEach",
        "keys" => "Map.keys",
        "values" => "Map.values",
        "entries" => "Map.entries",
        _ => return None,
    })
}

fn set_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "add" => "Set.add",
        "has" => "Set.has",
        "delete" => "Set.delete",
        "clear" => "Set.clear",
        "forEach" => "Set.forEach",
        "keys" => "Set.keys",
        "values" => "Set.values",
        "entries" => "Set.entries",
        _ => return None,
    })
}

fn date_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "getTime" => "Date.getTime",
        "valueOf" => "Date.valueOf",
        "toISOString" => "Date.toISOString",
        "toJSON" => "Date.toJSON",
        "toString" => "Date.toString",
        "getFullYear" | "getUTCFullYear" => "Date.getFullYear",
        "getMonth" | "getUTCMonth" => "Date.getMonth",
        "getDate" | "getUTCDate" => "Date.getDate",
        "getDay" | "getUTCDay" => "Date.getDay",
        "getHours" | "getUTCHours" => "Date.getHours",
        "getMinutes" | "getUTCMinutes" => "Date.getMinutes",
        "getSeconds" | "getUTCSeconds" => "Date.getSeconds",
        "getMilliseconds" | "getUTCMilliseconds" => "Date.getMilliseconds",
        "getTimezoneOffset" => "Date.getTimezoneOffset",
        _ => return None,
    })
}

fn as_array_ref(value: &Value, span: Span) -> Result<crate::value::ArrayRef, RuntimeError> {
    match value {
        Value::Array(array) => Ok(Rc::clone(array)),
        _ => Err(RuntimeError::type_error(
            "Array method requires an array receiver.",
            span,
        )),
    }
}

fn as_map_ref(value: &Value, span: Span) -> Result<crate::value::MapRef, RuntimeError> {
    match value {
        Value::Map(map) => Ok(Rc::clone(map)),
        _ => Err(RuntimeError::type_error(
            "Map method requires a Map receiver.",
            span,
        )),
    }
}

fn as_set_ref(value: &Value, span: Span) -> Result<crate::value::SetRef, RuntimeError> {
    match value {
        Value::Set(set) => Ok(Rc::clone(set)),
        _ => Err(RuntimeError::type_error(
            "Set method requires a Set receiver.",
            span,
        )),
    }
}

fn as_date_time(value: &Value, span: Span) -> Result<f64, RuntimeError> {
    match value {
        Value::Date(date) => Ok(date.time_ms),
        _ => Err(RuntimeError::type_error(
            "Date method requires a Date receiver.",
            span,
        )),
    }
}

fn date_component(
    value: &Value,
    span: Span,
    read: impl Fn(crate::date::DateParts) -> f64,
) -> Value {
    let time_ms = match as_date_time(value, span) {
        Ok(time_ms) => time_ms,
        Err(_) => return Value::Number(f64::NAN),
    };
    crate::date::parts(time_ms).map_or(Value::Number(f64::NAN), |parts| Value::Number(read(parts)))
}

fn as_string(value: &Value, span: Span) -> Result<String, RuntimeError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        _ => Err(RuntimeError::type_error(
            "String method requires a string receiver.",
            span,
        )),
    }
}

fn as_number(value: &Value, span: Span) -> Result<f64, RuntimeError> {
    match value {
        Value::Number(value) => Ok(*value),
        _ => Err(RuntimeError::type_error(
            "Number method requires a number receiver.",
            span,
        )),
    }
}

fn normalize_index(value: &Value, length: i64, fallback: i64) -> i64 {
    match value {
        Value::Undefined => fallback,
        _ => {
            let number = to_number(value);
            if number.is_nan() {
                0
            } else {
                let mut index = number.trunc() as i64;
                if index < 0 {
                    index = index.saturating_add(length);
                }
                index
            }
        }
    }
}

/// Absolute position clamp used by String search methods. Unlike Array search
/// methods, negative positions clamp to 0 instead of counting from the end.
fn absolute_index(value: &Value, length: i64, fallback: i64) -> i64 {
    match value {
        Value::Undefined => fallback,
        _ => {
            let number = to_number(value);
            if number.is_nan() {
                0
            } else {
                (number.trunc() as i64).clamp(0, length)
            }
        }
    }
}

fn clamp_index(number: f64, length: i64) -> i64 {
    if number.is_nan() {
        0
    } else {
        (number.trunc() as i64).clamp(0, length)
    }
}

fn flatten_into(items: &[Value], depth: usize, output: &mut Vec<Value>) {
    for item in items {
        match item {
            Value::Array(inner) if depth > 0 => {
                flatten_into(&inner.borrow(), depth - 1, output);
            }
            _ => output.push(item.clone()),
        }
    }
}

fn pad_string(text: &str, target: usize, padding: &str, start: bool) -> String {
    let length = text.chars().count();
    if length >= target || padding.is_empty() {
        return text.to_owned();
    }
    let needed = target - length;
    let mut pad = String::new();
    while pad.chars().count() < needed {
        pad.push_str(padding);
    }
    let pad: String = pad.chars().take(needed).collect();
    if start {
        format!("{pad}{text}")
    } else {
        format!("{text}{pad}")
    }
}

fn math_sign(number: f64) -> f64 {
    if number.is_nan() || number == 0.0 {
        number
    } else if number > 0.0 {
        1.0
    } else {
        -1.0
    }
}

/// Converts a string method index argument, treating `undefined` and `NaN` as
/// zero and rejecting negative or non-finite values.
fn string_index(value: &Value) -> Option<usize> {
    let number = to_number(value);
    if number.is_nan() {
        return Some(0);
    }
    let index = number.trunc();
    if index < 0.0 || index > usize::MAX as f64 {
        return None;
    }
    Some(index as usize)
}

/// Replaces `search` in `text`, expanding the JavaScript replacement patterns
/// `$$`, `$&`, `` $` ``, and `$'`.
fn replace_string(text: &str, search: &str, replacement: &str, all: bool) -> String {
    let mut output = String::new();
    if search.is_empty() {
        let mut cursor = 0;
        while cursor <= text.len() {
            let Some(character) = text[cursor..].chars().next() else {
                break;
            };
            let end = cursor + character.len_utf8();
            output.push_str(&expand_replacement(
                replacement,
                "",
                &text[..cursor],
                &text[cursor..],
            ));
            output.push(character);
            cursor = end;
            if !all {
                output.push_str(&text[cursor..]);
                return output;
            }
        }
        output.push_str(&expand_replacement(replacement, "", text, ""));
        return output;
    }

    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(search) {
        let start = cursor + relative;
        let end = start + search.len();
        output.push_str(&text[cursor..start]);
        output.push_str(&expand_replacement(
            replacement,
            search,
            &text[..start],
            &text[end..],
        ));
        cursor = end;
        if !all {
            break;
        }
    }
    output.push_str(&text[cursor..]);
    output
}

fn expand_replacement(replacement: &str, matched: &str, prefix: &str, suffix: &str) -> String {
    let mut output = String::new();
    let mut characters = replacement.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '$' {
            output.push(character);
            continue;
        }
        match characters.peek() {
            Some('$') => {
                characters.next();
                output.push('$');
            }
            Some('&') => {
                characters.next();
                output.push_str(matched);
            }
            Some('`') => {
                characters.next();
                output.push_str(prefix);
            }
            Some('\'') => {
                characters.next();
                output.push_str(suffix);
            }
            _ => output.push('$'),
        }
    }
    output
}

fn math_hypot(arguments: &[Value]) -> f64 {
    let values = arguments.iter().map(to_number).collect::<Vec<_>>();
    let mut maximum = 0.0f64;
    let mut any_nan = false;
    for value in &values {
        if value.is_infinite() {
            return f64::INFINITY;
        }
        if value.is_nan() {
            any_nan = true;
        }
        let absolute = value.abs();
        if absolute > maximum {
            maximum = absolute;
        }
    }
    if any_nan {
        return f64::NAN;
    }
    if maximum == 0.0 {
        return 0.0;
    }
    maximum
        * values
            .iter()
            .map(|value| (value / maximum).powi(2))
            .sum::<f64>()
            .sqrt()
}

fn math_extreme(arguments: &[Value], maximum: bool) -> f64 {
    let mut result = if maximum {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    };
    for value in arguments {
        let number = to_number(value);
        if number.is_nan() {
            return f64::NAN;
        }
        if (maximum && number > result) || (!maximum && number < result) {
            result = number;
        }
    }
    result
}

fn parse_int(text: &str, radix: f64) -> f64 {
    let text = text.trim_start_matches(is_js_whitespace);
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => match text.strip_prefix('+') {
            Some(rest) => (1.0, rest),
            None => (1.0, text),
        },
    };
    let requested = to_int32(radix);
    let automatic = requested == 0;
    if !automatic && !(2..=36).contains(&requested) {
        return f64::NAN;
    }
    let base = if automatic { 10 } else { requested as u32 };
    let hexadecimal = rest.starts_with("0x") || rest.starts_with("0X");
    let (base, digits) = if (automatic || base == 16) && hexadecimal {
        (16, &rest[2..])
    } else {
        (base, rest)
    };
    let end = digits
        .char_indices()
        .find(|(_, character)| character.to_digit(base).is_none())
        .map_or(digits.len(), |(index, _)| index);
    let digits = &digits[..end];
    if digits.is_empty() {
        return f64::NAN;
    }
    // Decimal input goes through the correctly-rounded parser. Other bases
    // accumulate exactly in `u128` and fall back to floating point only when
    // the digit string outgrows it.
    if base == 10 {
        return sign * digits.parse::<f64>().unwrap_or(f64::NAN);
    }
    let mut exact = Some(0u128);
    let mut value = 0.0f64;
    for character in digits.chars() {
        let digit = character.to_digit(base).expect("digits are validated") as u128;
        if let Some(current) = exact {
            match current
                .checked_mul(u128::from(base))
                .and_then(|next| next.checked_add(digit))
            {
                Some(next) => {
                    exact = Some(next);
                    continue;
                }
                None => {
                    value = current as f64;
                    exact = None;
                }
            }
        }
        value = value * f64::from(base) + digit as f64;
    }
    sign * exact.map_or(value, |current| current as f64)
}

fn parse_float(text: &str) -> f64 {
    let text = text.trim_start_matches(is_js_whitespace);
    if text.is_empty() {
        return f64::NAN;
    }
    let mut best = None;
    for (index, _) in text.char_indices().skip(1) {
        if let Ok(value) = text[..index].parse::<f64>() {
            best = Some(value);
        }
    }
    match text.parse::<f64>() {
        Ok(value) => value,
        Err(_) => best.unwrap_or(f64::NAN),
    }
}

fn string_to_number(text: &str) -> f64 {
    let text = text.trim_matches(is_js_whitespace);
    if text.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = text.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            return match u128::from_str_radix(digits, radix) {
                Ok(value) => value as f64,
                Err(_) => f64::NAN,
            };
        }
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

/// Upper bound for `Array.from` array-like materialization, guarding against
/// one-shot allocation that the timeout cannot interrupt.
const MAX_ARRAY_LIKE_LENGTH: usize = 1_000_000;

fn array_like_items(value: &Value) -> Option<Vec<Value>> {
    let Value::Object(object) = value else {
        return None;
    };
    let length = match object.borrow().get("length") {
        Some(Value::Number(length)) if length.is_finite() && *length >= 0.0 => {
            length.trunc() as usize
        }
        _ => return None,
    };
    if length > MAX_ARRAY_LIKE_LENGTH {
        return None;
    }
    let object = object.borrow();
    Some(
        (0..length)
            .map(|index| {
                object
                    .get(&index.to_string())
                    .cloned()
                    .unwrap_or(Value::Undefined)
            })
            .collect(),
    )
}

enum CoercionIssue {
    NonData,
    Promise,
}

/// Rejects callables and un-awaited promises at coercion inputs, matching
/// upstream's use of the data boundary. Sandbox values (Date, Map, Set, errors)
/// stay data; tool references are left to their existing rendering.
fn coercion_issue(value: &Value, seen: &mut Vec<usize>) -> Option<CoercionIssue> {
    match value {
        Value::Function(_) | Value::Builtin(_) | Value::BoundMethod { .. } => {
            Some(CoercionIssue::NonData)
        }
        Value::Promise(_) => Some(CoercionIssue::Promise),
        Value::Array(items) => {
            let identity = Rc::as_ptr(items) as *const () as usize;
            if seen.contains(&identity) {
                return None;
            }
            seen.push(identity);
            let issue = items
                .borrow()
                .iter()
                .find_map(|item| coercion_issue(item, seen));
            seen.pop();
            issue
        }
        Value::Object(object) => {
            let identity = Rc::as_ptr(object) as *const () as usize;
            if seen.contains(&identity) {
                return None;
            }
            seen.push(identity);
            let issue = object
                .borrow()
                .values()
                .find_map(|item| coercion_issue(item, seen));
            seen.pop();
            issue
        }
        _ => None,
    }
}

/// Mirrors upstream `containsOpaqueReference`: functions, tool references and
/// un-awaited promises are interpreter machinery, not data operands. Sandbox
/// values (Date, Map, Set, errors) are data even when they contain callables.
fn contains_opaque_reference(value: &Value, seen: &mut Vec<usize>) -> bool {
    match value {
        Value::Function(_)
        | Value::Builtin(_)
        | Value::BoundMethod { .. }
        | Value::ToolRef(_)
        | Value::Promise(_) => true,
        Value::Array(items) => {
            let identity = Rc::as_ptr(items) as *const () as usize;
            if seen.contains(&identity) {
                return false;
            }
            seen.push(identity);
            let found = items
                .borrow()
                .iter()
                .any(|item| contains_opaque_reference(item, seen));
            seen.pop();
            found
        }
        Value::Object(object) => {
            let identity = Rc::as_ptr(object) as *const () as usize;
            if seen.contains(&identity) {
                return false;
            }
            seen.push(identity);
            let found = object
                .borrow()
                .values()
                .any(|item| contains_opaque_reference(item, seen));
            seen.pop();
            found
        }
        _ => false,
    }
}

fn own_keys(value: &Value) -> Vec<String> {
    match value {
        Value::Object(object) => ordered_keys(&object.borrow()),
        Value::Array(array) => (0..array.borrow().len())
            .map(|index| index.to_string())
            .collect(),
        Value::String(value) => (0..value.chars().count())
            .map(|index| index.to_string())
            .collect(),
        Value::Error(_) => vec!["name".to_owned(), "message".to_owned()],
        _ => Vec::new(),
    }
}

/// Whitespace trimmed by `String.prototype.trim`, including the BOM that
/// `char::is_whitespace` does not cover.
fn is_js_whitespace(character: char) -> bool {
    character.is_whitespace() || character == '\u{FEFF}'
}

fn json_to_value(json: &serde_json::Value, span: Span) -> Result<Value, RuntimeError> {
    json_to_value_with_overflow(json, span, false)
}

/// Placeholders used while parsing JSON numeric literals that overflow `f64`.
///
/// `serde_json` rejects `1e400`, while ECMAScript `JSON.parse` returns
/// `Infinity`. The scanner below replaces only those literals, so the sentinels
/// can be recognized after a successful parse.
const POSITIVE_INFINITY_SENTINEL: &str = "__opencode_codemode_positive_infinity__";
const NEGATIVE_INFINITY_SENTINEL: &str = "__opencode_codemode_negative_infinity__";

/// Rewrites overflowing JSON number literals to sentinel strings.
///
/// Returns `None` when every number fits in `f64`, which keeps the common path
/// identical to a plain `serde_json` parse. Strings are copied verbatim so
/// digits and `-` inside string values are never touched.
fn repair_overflow_numbers(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut output = String::with_capacity(source.len() + 16);
    let mut index = 0;
    let mut changed = false;

    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index = (index + 2).min(bytes.len()),
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                output.push_str(&source[start..index]);
            }
            b'-' | b'0'..=b'9' => {
                let start = index;
                if bytes[index] == b'-' {
                    index += 1;
                }
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
                if index < bytes.len() && bytes[index] == b'.' {
                    index += 1;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                }
                if index < bytes.len() && matches!(bytes[index], b'e' | b'E') {
                    index += 1;
                    if index < bytes.len() && matches!(bytes[index], b'+' | b'-') {
                        index += 1;
                    }
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                }
                let token = &source[start..index];
                match token.parse::<f64>() {
                    Ok(value) if value.is_infinite() => {
                        output.push('"');
                        output.push_str(if value.is_sign_negative() {
                            NEGATIVE_INFINITY_SENTINEL
                        } else {
                            POSITIVE_INFINITY_SENTINEL
                        });
                        output.push('"');
                        changed = true;
                    }
                    _ => output.push_str(token),
                }
            }
            _ => {
                let character = source[index..]
                    .chars()
                    .next()
                    .expect("index is on a character boundary");
                output.push(character);
                index += character.len_utf8();
            }
        }
    }

    changed.then_some(output)
}

fn json_to_value_with_overflow(
    json: &serde_json::Value,
    span: Span,
    allow_overflow_sentinels: bool,
) -> Result<Value, RuntimeError> {
    use serde_json::Value as Json;
    Ok(match json {
        Json::Null => Value::Null,
        Json::Bool(value) => Value::Boolean(*value),
        Json::Number(value) => Value::Number(value.as_f64().unwrap_or(f64::NAN)),
        Json::String(value) if allow_overflow_sentinels && value == POSITIVE_INFINITY_SENTINEL => {
            Value::Number(f64::INFINITY)
        }
        Json::String(value) if allow_overflow_sentinels && value == NEGATIVE_INFINITY_SENTINEL => {
            Value::Number(f64::NEG_INFINITY)
        }
        Json::String(value) => Value::String(value.clone()),
        Json::Array(items) => Value::array(
            items
                .iter()
                .map(|item| json_to_value_with_overflow(item, span, allow_overflow_sentinels))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Json::Object(map) => {
            let mut object = IndexMap::new();
            for (key, value) in map {
                if is_blocked_member(key) {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::InvalidDataValue,
                        format!("Property '{key}' is not available in CodeMode."),
                        span,
                    ));
                }
                object.insert(
                    key.clone(),
                    json_to_value_with_overflow(value, span, allow_overflow_sentinels)?,
                );
            }
            Value::Object(Rc::new(RefCell::new(object)))
        }
    })
}

fn tool_error_value(error: &ToolError, span: Span) -> Value {
    let mut entries = vec![("message", Value::String(error.message().to_owned()))];
    if let Some(data) = error.data() {
        if let Ok(value) = json_to_value(data, span) {
            entries.push(("data", value));
        }
    }
    Value::from_entries(entries)
}

#[derive(Clone, Copy)]
enum JsonStringifyError {
    Circular,
    TooDeep,
    NonData,
    UnawaitedPromise,
}

fn stringify_json_with_indent(
    value: &Value,
    indent: &str,
) -> Result<Option<String>, JsonStringifyError> {
    stringify_json_inner(value, &mut Vec::new(), indent, 0)
}

fn stringify_json_inner(
    value: &Value,
    ancestors: &mut Vec<usize>,
    indent: &str,
    depth: usize,
) -> Result<Option<String>, JsonStringifyError> {
    match value {
        Value::Undefined | Value::ToolRef(_) => Ok(None),
        Value::Function(_) | Value::Builtin(_) | Value::BoundMethod { .. } => {
            Err(JsonStringifyError::NonData)
        }
        Value::Error(error) => {
            let name = serde_json::to_string(error.kind.name()).unwrap_or_default();
            let message = serde_json::to_string(&error.message).unwrap_or_default();
            if indent.is_empty() {
                Ok(Some(format!("{{\"name\":{name},\"message\":{message}}}")))
            } else {
                let inner = indent.repeat(depth + 1);
                let outer = indent.repeat(depth);
                Ok(Some(format!(
                    "{{\n{inner}\"name\": {name},\n{inner}\"message\": {message}\n{outer}}}"
                )))
            }
        }
        Value::Promise(_) => Err(JsonStringifyError::UnawaitedPromise),
        Value::Map(_) | Value::Set(_) => Ok(Some("{}".to_owned())),
        Value::Date(date) => Ok(Some(
            crate::date::iso_string(date.time_ms)
                .and_then(|text| serde_json::to_string(&text).ok())
                .unwrap_or_else(|| "null".to_owned()),
        )),
        Value::Null => Ok(Some("null".to_owned())),
        Value::Boolean(value) => Ok(Some(value.to_string())),
        Value::Number(value) => Ok(Some(if value.is_finite() {
            format_number(*value)
        } else {
            "null".to_owned()
        })),
        Value::String(value) => Ok(Some(
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned()),
        )),
        Value::Array(items) => {
            let identity = Rc::as_ptr(items) as *const () as usize;
            if ancestors.contains(&identity) {
                return Err(JsonStringifyError::Circular);
            }
            if ancestors.len() >= crate::value::MAX_VALUE_DEPTH * 2 {
                return Err(JsonStringifyError::TooDeep);
            }
            ancestors.push(identity);
            let mut parts = Vec::new();
            for item in items.borrow().iter() {
                parts.push(
                    match stringify_json_inner(item, ancestors, indent, depth + 1)? {
                        Some(text) => text,
                        None => "null".to_owned(),
                    },
                );
            }
            ancestors.pop();
            if indent.is_empty() {
                Ok(Some(format!("[{}]", parts.join(","))))
            } else if parts.is_empty() {
                Ok(Some("[]".to_owned()))
            } else {
                let inner = indent.repeat(depth + 1);
                let separator = format!(",\n{inner}");
                Ok(Some(format!(
                    "[\n{inner}{}\n{}]",
                    parts.join(&separator),
                    indent.repeat(depth)
                )))
            }
        }
        Value::Object(object) => {
            let identity = Rc::as_ptr(object) as *const () as usize;
            if ancestors.contains(&identity) {
                return Err(JsonStringifyError::Circular);
            }
            if ancestors.len() >= crate::value::MAX_VALUE_DEPTH * 2 {
                return Err(JsonStringifyError::TooDeep);
            }
            ancestors.push(identity);
            let source = object.borrow();
            let mut parts = Vec::new();
            for key in ordered_keys(&source) {
                let value = source.get(&key).expect("ordered keys exist");
                let Some(serialized) = stringify_json_inner(value, ancestors, indent, depth + 1)?
                else {
                    continue;
                };
                let key = serde_json::to_string(&key).unwrap_or_default();
                if indent.is_empty() {
                    parts.push(format!("{key}:{serialized}"));
                } else {
                    parts.push(format!("{key}: {serialized}"));
                }
            }
            ancestors.pop();
            if indent.is_empty() {
                Ok(Some(format!("{{{}}}", parts.join(","))))
            } else if parts.is_empty() {
                Ok(Some("{}".to_owned()))
            } else {
                let inner = indent.repeat(depth + 1);
                let separator = format!(",\n{inner}");
                Ok(Some(format!(
                    "{{\n{inner}{}\n{}}}",
                    parts.join(&separator),
                    indent.repeat(depth)
                )))
            }
        }
    }
}

/// Coerces an optional method digit argument, enforcing the JavaScript range.
fn optional_digits(
    value: Option<&Value>,
    method: &str,
    minimum: usize,
    maximum: usize,
    span: Span,
) -> Result<Option<usize>, RuntimeError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_undefined() {
        return Ok(None);
    }
    let number = to_number(value);
    let digits = if number.is_nan() { 0.0 } else { number.trunc() };
    if digits < minimum as f64 || digits > maximum as f64 {
        return Err(RuntimeError::new(
            RuntimeErrorKind::RangeError,
            format!("{method}() digits argument must be between {minimum} and {maximum}."),
            span,
        ));
    }
    Ok(Some(digits as usize))
}

/// Formats a number with `precision` significant digits, matching the fixed
/// and exponential selection rules of `Number.prototype.toPrecision`.
/// Decimal exponent of a positive, finite, non-zero value.
fn decimal_exponent(value: f64) -> i32 {
    // Use a high-precision expansion instead of the shortest representation.
    // The double nearest 1e-7 is slightly below it, so its exact exponent is
    // -8 even though Rust's `{:e}` prints `1e-7`. `toPrecision` and
    // `toExponential` must round against the exact value.
    format!("{value:.40e}")
        .split_once('e')
        .and_then(|(_, exponent)| exponent.parse().ok())
        .unwrap_or(0)
}

/// Decimal exponent of a fixed-notation string such as `"0.0012"` or `"1200"`.
fn fixed_exponent(text: &str) -> i32 {
    let (integer, fraction) = text.split_once('.').unwrap_or((text, ""));
    if let Some(index) = integer.bytes().position(|byte| byte != b'0') {
        return (integer.len() - 1 - index) as i32;
    }
    match fraction.bytes().position(|byte| byte != b'0') {
        Some(index) => -((index as i32) + 1),
        None => 0,
    }
}

/// Extracts `count` significant digits, padding with zeros when a rounded value
/// ends before the requested precision.
fn significant_digits(text: &str, count: usize) -> String {
    let mut digits = text
        .bytes()
        .filter(|byte| byte.is_ascii_digit())
        .skip_while(|byte| *byte == b'0')
        .map(|byte| byte as char)
        .collect::<String>();
    if digits.is_empty() {
        digits.push('0');
    }
    while digits.len() < count {
        digits.push('0');
    }
    digits.truncate(count);
    digits
}

/// Rounds a positive, finite value to `places` fractional digits (a negative
/// `places` rounds integer digits) with ECMAScript's round-half-away rule.
/// Rust's formatter rounds half to even, so the decision digit is read from a
/// guarded exact expansion and the carry is applied to the decimal string.
fn round_decimal(value: f64, places: i32) -> String {
    if value == 0.0 {
        return if places > 0 {
            format!("0.{}", "0".repeat(places as usize))
        } else {
            "0".to_owned()
        };
    }
    const GUARD: usize = 40;
    let precision = places.max(0) as usize + GUARD;
    let probe = format!("{value:.precision$}");
    let (integer, fraction) = probe.split_once('.').unwrap_or((probe.as_str(), ""));
    let mut digits = integer.bytes().chain(fraction.bytes()).collect::<Vec<u8>>();
    let mut point = integer.len() as i64;
    let decision = point + i64::from(places);
    let mut kept = decision.max(0) as usize;
    let round_up =
        decision >= 0 && (decision as usize) < digits.len() && digits[decision as usize] >= b'5';
    if round_up {
        if kept == 0 {
            digits.clear();
            digits.push(b'1');
            point = 1 - i64::from(places);
            kept = 1;
        } else {
            let mut cursor = kept;
            loop {
                if cursor == 0 {
                    digits.insert(0, b'1');
                    point += 1;
                    kept += 1;
                    break;
                }
                cursor -= 1;
                if digits[cursor] == b'9' {
                    digits[cursor] = b'0';
                } else {
                    digits[cursor] += 1;
                    break;
                }
            }
        }
    }
    digits.truncate(kept);
    if digits.is_empty() {
        return if places > 0 {
            format!("0.{}", "0".repeat(places as usize))
        } else {
            "0".to_owned()
        };
    }
    if point <= 0 {
        let mut output = String::from("0.");
        output.push_str(&"0".repeat((-point) as usize));
        output.push_str(&String::from_utf8_lossy(&digits));
        output
    } else if point as usize >= digits.len() {
        let mut output = String::from_utf8_lossy(&digits).into_owned();
        output.push_str(&"0".repeat(point as usize - digits.len()));
        output
    } else {
        let (integer, fraction) = digits.split_at(point as usize);
        format!(
            "{}.{}",
            String::from_utf8_lossy(integer),
            String::from_utf8_lossy(fraction)
        )
    }
}

/// Formats a positive, finite value in exponential notation with `digits`
/// fractional mantissa digits.
fn exponential_from_fixed(value: f64, digits: usize) -> String {
    let places = digits as i32 - decimal_exponent(value);
    let rounded = round_decimal(value, places);
    let exponent = fixed_exponent(&rounded);
    let significant = significant_digits(&rounded, digits + 1);
    let mut output = String::new();
    output.push(significant.as_bytes()[0] as char);
    if digits > 0 {
        output.push('.');
        output.push_str(&significant[1..]);
    }
    output.push('e');
    if exponent >= 0 {
        output.push('+');
    }
    output.push_str(&exponent.to_string());
    output
}

fn number_to_precision(value: f64, precision: usize) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-Infinity" } else { "Infinity" }.to_owned();
    }
    let negative = value < 0.0;
    let value = value.abs();
    if value == 0.0 {
        return if precision == 1 {
            "0".to_owned()
        } else {
            format!("0.{}", "0".repeat(precision - 1))
        };
    }

    let places = precision as i32 - 1 - decimal_exponent(value);
    let rounded = round_decimal(value, places);
    let exponent = fixed_exponent(&rounded);
    let digits = significant_digits(&rounded, precision);

    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if exponent < -6 || exponent >= precision as i32 {
        output.push(digits.as_bytes()[0] as char);
        if precision > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        output.push('e');
        if exponent >= 0 {
            output.push('+');
        }
        output.push_str(&exponent.to_string());
    } else if exponent >= 0 {
        let point = exponent as usize + 1;
        output.push_str(&digits[..point]);
        if point < precision {
            output.push('.');
            output.push_str(&digits[point..]);
        }
    } else {
        output.push_str("0.");
        output.push_str(&"0".repeat((-exponent - 1) as usize));
        output.push_str(&digits);
    }
    output
}

/// Formats a number in exponential notation. Without `digits`, the shortest
/// round-trip mantissa is used.
fn number_to_exponential(value: f64, digits: Option<usize>) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-Infinity" } else { "Infinity" }.to_owned();
    }
    if value == 0.0 {
        return match digits {
            None | Some(0) => "0e+0".to_owned(),
            Some(digits) => format!("0.{}e+0", "0".repeat(digits)),
        };
    }
    if let Some(digits) = digits {
        let text = exponential_from_fixed(value.abs(), digits);
        return if value < 0.0 {
            format!("-{text}")
        } else {
            text
        };
    }
    let text = format!("{value:e}");
    match text.split_once('e') {
        Some((mantissa, exponent)) => {
            let exponent: i32 = exponent.parse().unwrap_or(0);
            if exponent >= 0 {
                format!("{mantissa}e+{exponent}")
            } else {
                format!("{mantissa}e{exponent}")
            }
        }
        None => text,
    }
}

fn number_to_string(number: f64, radix: u32) -> String {
    if radix == 10 || !(2..=36).contains(&radix) || !number.is_finite() || number.fract() != 0.0 {
        return format_number(number);
    }
    let negative = number < 0.0;
    let mut value = number.abs() as u128;
    if value == 0 {
        return "0".to_owned();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut output = Vec::new();
    while value > 0 {
        output.push(digits[(value % u128::from(radix)) as usize]);
        value /= u128::from(radix);
    }
    if negative {
        output.push(b'-');
    }
    output.reverse();
    String::from_utf8(output).unwrap_or_default()
}

/// Parses and evaluates a program.
pub fn execute<'a>(program: &'a Program<'a>) -> Result<Value, RuntimeError> {
    let mut interpreter = Interpreter::<'a>::new();
    interpreter.run(program)
}

/// Parses and evaluates a program with a host for `tools.*` references.
pub fn execute_with_host<'a>(
    program: &'a Program<'a>,
    host: Box<dyn ToolHost>,
) -> Result<Value, RuntimeError> {
    let mut interpreter = Interpreter::<'a>::new();
    interpreter.set_tool_host(host);
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
        Value::Map(map) => Some(
            map.entries()
                .into_iter()
                .map(|(key, value)| Value::array(vec![key, value]))
                .collect(),
        ),
        Value::Set(set) => Some(set.values()),
        _ => None,
    }
}

fn iterable_items(value: &Value, span: Span) -> Result<Vec<Value>, RuntimeError> {
    spread_items(value).ok_or_else(|| {
        RuntimeError::type_error("Loop target must be an array or string in CodeMode.", span)
    })
}

fn enumerable_keys(value: &Value, span: Span) -> Result<Vec<Value>, RuntimeError> {
    match value {
        Value::Object(object) => Ok(ordered_keys(&object.borrow())
            .into_iter()
            .map(Value::String)
            .collect()),
        Value::Array(array) => Ok((0..array.borrow().len())
            .map(|index| Value::String(index.to_string()))
            .collect()),
        // Upstream errors are null-prototype data objects whose `name` and
        // `message` properties are enumerable, so `for...in` sees both.
        Value::Error(_) => Ok(vec![
            Value::String("name".to_owned()),
            Value::String("message".to_owned()),
        ]),
        _ => Err(RuntimeError::type_error(
            "for...in requires a plain object, array, or tools reference. \
             Use for...of for arrays/strings/Maps/Sets, or Object.keys(value).",
            span,
        )),
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
        Value::Error(_) => key == "name" || key == "message",
        Value::Map(_) => key == "size" || map_method(key).is_some(),
        Value::Set(_) => key == "size" || set_method(key).is_some(),
        Value::Date(_) => date_method(key).is_some(),
        _ => false,
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Boolean(value) => *value,
        Value::Number(value) => *value != 0.0 && !value.is_nan(),
        Value::String(value) => !value.is_empty(),
        Value::Array(_)
        | Value::Object(_)
        | Value::Function(_)
        | Value::Builtin(_)
        | Value::BoundMethod { .. }
        | Value::ToolRef(_)
        | Value::Error(_)
        | Value::Promise(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Date(_) => true,
    }
}

fn strict_equals(left: &Value, right: &Value) -> bool {
    left == right
}

fn is_object_like(value: &Value) -> bool {
    !matches!(
        value,
        Value::Undefined | Value::Null | Value::Boolean(_) | Value::Number(_) | Value::String(_)
    )
}

/// Coerces an operand of `==`. Two objects compare by identity before this
/// runs; a lone object stringifies except for `Date`, which upstream turns into
/// its time value for every operator except `+`.
fn equality_operand(value: &Value) -> Value {
    match value {
        Value::Date(date) => Value::Number(date.time_ms),
        Value::Undefined
        | Value::Null
        | Value::Boolean(_)
        | Value::Number(_)
        | Value::String(_) => value.clone(),
        _ => Value::String(to_js_string(value)),
    }
}

fn loose_equals(left: &Value, right: &Value) -> bool {
    if is_object_like(left) && is_object_like(right) {
        return strict_equals(left, right);
    }
    primitive_loose_equals(&equality_operand(left), &equality_operand(right))
}

fn primitive_loose_equals(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null | Value::Undefined, Value::Null | Value::Undefined) => true,
        (Value::Number(left), Value::Number(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        (Value::Number(left), Value::String(right)) => *left == string_to_number(right),
        (Value::String(left), Value::Number(right)) => string_to_number(left) == *right,
        (Value::Boolean(_), _) => primitive_loose_equals(&Value::Number(to_number(left)), right),
        (_, Value::Boolean(_)) => primitive_loose_equals(left, &Value::Number(to_number(right))),
        _ => false,
    }
}

/// Compares strings by UTF-16 code units, as JavaScript relational operators do.
fn utf16_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// Coerces an operand of `+`: objects stringify, including `Date`.
fn addition_operand(value: &Value) -> Value {
    match value {
        Value::Undefined
        | Value::Null
        | Value::Boolean(_)
        | Value::Number(_)
        | Value::String(_) => value.clone(),
        _ => Value::String(to_js_string(value)),
    }
}

/// Coerces an operand of a relational comparison. Upstream maps `Date` to its
/// time value for ordering while other objects stringify.
fn comparison_operand(value: &Value) -> Value {
    match value {
        Value::Date(date) => Value::Number(date.time_ms),
        Value::Undefined
        | Value::Null
        | Value::Boolean(_)
        | Value::Number(_)
        | Value::String(_) => value.clone(),
        _ => Value::String(to_js_string(value)),
    }
}

fn compare(left: &Value, right: &Value, _span: Span) -> Result<f64, RuntimeError> {
    let left = comparison_operand(left);
    let right = comparison_operand(right);
    match (&left, &right) {
        (Value::String(left), Value::String(right)) => Ok(match utf16_cmp(left, right) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }),
        _ => {
            let left = to_number(&left);
            let right = to_number(&right);
            Ok(if left.is_nan() || right.is_nan() {
                f64::NAN
            } else if left < right {
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
        Value::String(value) => string_to_number(value),
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
        Value::Object(_)
        | Value::Function(_)
        | Value::Builtin(_)
        | Value::BoundMethod { .. }
        | Value::Error(_)
        | Value::Promise(_)
        | Value::Map(_)
        | Value::Set(_) => f64::NAN,
        Value::Date(date) => date.time_ms,
        Value::ToolRef(_) => f64::NAN,
    }
}

fn contains_identity(value: &Value, target: usize, visited: &mut Vec<usize>) -> bool {
    match value {
        Value::Array(items) => {
            let identity = Rc::as_ptr(items) as *const () as usize;
            if identity == target {
                return true;
            }
            if visited.contains(&identity) {
                return false;
            }
            visited.push(identity);
            let found = items
                .borrow()
                .iter()
                .any(|item| contains_identity(item, target, visited));
            visited.pop();
            found
        }
        Value::Object(object) => {
            let identity = Rc::as_ptr(object) as *const () as usize;
            if identity == target {
                return true;
            }
            if visited.contains(&identity) {
                return false;
            }
            visited.push(identity);
            let found = object
                .borrow()
                .values()
                .any(|item| contains_identity(item, target, visited));
            visited.pop();
            found
        }
        _ => false,
    }
}

/// Maximum console nesting before values render as `"..."`.
const MAX_CONSOLE_DEPTH: usize = 32;

/// Formats one console argument. Top-level strings print bare, matching the
/// upstream console formatter.
fn format_console_argument(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".to_owned(),
        Value::String(text) => text.clone(),
        _ => format_console_value(value, &mut Vec::new(), 0),
    }
}

/// Formats a value the way a debugger would: sandbox values keep their
/// friendly forms at any depth, cycles render `"[Circular]"`, and runtime
/// references render `"[CodeMode reference]"`.
fn format_console_value(value: &Value, seen: &mut Vec<usize>, depth: usize) -> String {
    if depth > MAX_CONSOLE_DEPTH {
        return "...".to_owned();
    }
    match value {
        Value::Undefined | Value::Null => "null".to_owned(),
        Value::Boolean(value) => value.to_string(),
        Value::Number(value) => format_number(*value),
        Value::String(value) => serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned()),
        Value::Function(_) | Value::Builtin(_) | Value::BoundMethod { .. } | Value::ToolRef(_) => {
            "[CodeMode reference]".to_owned()
        }
        Value::Promise(_) => "[Promise (await it to get its value)]".to_owned(),
        Value::Date(date) => {
            crate::date::iso_string(date.time_ms).unwrap_or_else(|| "Invalid Date".to_owned())
        }
        Value::Error(error) => {
            let name = serde_json::to_string(error.kind.name()).unwrap_or_default();
            let message = serde_json::to_string(&error.message).unwrap_or_default();
            format!("{{\"name\":{name},\"message\":{message}}}")
        }
        Value::Array(array) => {
            let pointer = Rc::as_ptr(array) as *const () as usize;
            if seen.contains(&pointer) {
                return "[Circular]".to_owned();
            }
            seen.push(pointer);
            let items = array
                .borrow()
                .iter()
                .map(|item| format_console_value(item, seen, depth + 1))
                .collect::<Vec<_>>()
                .join(",");
            seen.pop();
            format!("[{items}]")
        }
        Value::Object(object) => {
            let pointer = Rc::as_ptr(object) as *const () as usize;
            if seen.contains(&pointer) {
                return "[Circular]".to_owned();
            }
            seen.push(pointer);
            let source = object.borrow();
            let entries = ordered_keys(&source)
                .into_iter()
                .map(|key| {
                    let value = source.get(&key).expect("ordered keys exist");
                    let name = serde_json::to_string(&key).unwrap_or_default();
                    format!("{name}:{}", format_console_value(value, seen, depth + 1))
                })
                .collect::<Vec<_>>()
                .join(",");
            seen.pop();
            format!("{{{entries}}}")
        }
        Value::Map(map) => {
            let pointer = Rc::as_ptr(map) as *const () as usize;
            if seen.contains(&pointer) {
                return "[Circular]".to_owned();
            }
            seen.push(pointer);
            let entries = map
                .entries()
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "[{},{}]",
                        format_console_value(&key, seen, depth + 1),
                        format_console_value(&value, seen, depth + 1)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            seen.pop();
            format!("Map({}) [{}]", map.len(), entries)
        }
        Value::Set(set) => {
            let pointer = Rc::as_ptr(set) as *const () as usize;
            if seen.contains(&pointer) {
                return "[Circular]".to_owned();
            }
            seen.push(pointer);
            let values = set
                .values()
                .into_iter()
                .map(|value| format_console_value(&value, seen, depth + 1))
                .collect::<Vec<_>>()
                .join(",");
            seen.pop();
            format!("Set({}) [{}]", set.len(), values)
        }
    }
}

/// Renders `console.table` for an array of plain objects.
fn format_console_table(data: Option<&Value>, columns: Option<&Value>) -> String {
    let Some(Value::Array(rows)) = data else {
        return match data {
            Some(value) => format_console_argument(value),
            None => "undefined".to_owned(),
        };
    };
    let rows = rows.borrow();

    let mut names = Vec::new();
    if let Some(Value::Array(columns)) = columns {
        for column in columns.borrow().iter() {
            if let Value::String(name) = column {
                names.push(name.clone());
            }
        }
    }
    if names.is_empty() {
        for row in rows.iter() {
            if let Value::Object(object) = row {
                for key in object.borrow().keys() {
                    if !names.contains(key) {
                        names.push(key.clone());
                    }
                }
            }
        }
    }
    if names.is_empty() {
        return data.map_or_else(|| "undefined".to_owned(), format_console_argument);
    }

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!("(index)\t{}", names.join("\t")));
    for (index, row) in rows.iter().enumerate() {
        let cells = names
            .iter()
            .map(|name| {
                let value = match row {
                    Value::Object(object) => object
                        .borrow()
                        .get(name)
                        .cloned()
                        .unwrap_or(Value::Undefined),
                    _ => Value::Undefined,
                };
                format_console_value(&value, &mut Vec::new(), 0)
            })
            .collect::<Vec<_>>()
            .join("\t");
        lines.push(format!("{index}\t{cells}"));
    }
    lines.join("\n")
}

/// ECMAScript `Number::exponentiate`, including the special cases that Rust's
/// `powf` does not share (notably `(+-1) ** Infinity === NaN` and signed zero).
fn js_pow(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() {
        return f64::NAN;
    }
    if exponent == 0.0 {
        return 1.0;
    }
    if base.is_nan() {
        return f64::NAN;
    }
    if base.abs() == 1.0 && exponent.is_infinite() {
        return f64::NAN;
    }
    if base.is_infinite() {
        if base > 0.0 {
            return if exponent > 0.0 { f64::INFINITY } else { 0.0 };
        }
        let odd = is_odd_integer(exponent);
        if exponent > 0.0 {
            return if odd {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        return if odd { -0.0 } else { 0.0 };
    }
    if base == 0.0 {
        let negative = base.is_sign_negative();
        let odd = is_odd_integer(exponent);
        if exponent > 0.0 {
            return if negative && odd { -0.0 } else { 0.0 };
        }
        return if negative && odd {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if base < 0.0 && exponent.is_finite() && exponent.fract() != 0.0 {
        return f64::NAN;
    }
    base.powf(exponent)
}

/// Finite integral values above 2^53 are all even, so only smaller integers can
/// be odd. Saturating casts would otherwise report them as odd.
fn is_odd_integer(value: f64) -> bool {
    value.is_finite()
        && value.fract() == 0.0
        && value.abs() < 9_007_199_254_740_992.0
        && (value as i64) % 2 != 0
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

/// Maximum string length accepted by allocation-heavy string methods, mirroring
/// the engine string limit closely enough to reject runaway requests.
const MAX_STRING_LENGTH: usize = 536_870_888;

fn to_js_string(value: &Value) -> String {
    to_js_string_seen(value, &mut Vec::new())
}

fn to_js_string_seen(value: &Value, seen: &mut Vec<usize>) -> String {
    match value {
        Value::Undefined => "undefined".to_owned(),
        Value::Null => "null".to_owned(),
        Value::Boolean(value) => value.to_string(),
        Value::Number(value) => format_number(*value),
        Value::String(value) => value.clone(),
        Value::Array(array) => {
            let pointer = Rc::as_ptr(array) as *const () as usize;
            if seen.contains(&pointer) {
                return String::new();
            }
            seen.push(pointer);
            let text = array
                .borrow()
                .iter()
                .map(|item| {
                    if item.is_nullish() {
                        String::new()
                    } else {
                        to_js_string_seen(item, seen)
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            seen.pop();
            text
        }
        Value::Object(_) => "[object Object]".to_owned(),
        Value::Function(_) | Value::Builtin(_) | Value::BoundMethod { .. } => {
            "[Function]".to_owned()
        }
        Value::Error(_) => "[object Object]".to_owned(),
        Value::Promise(_) => "[object Promise]".to_owned(),
        Value::Map(_) => "[object Map]".to_owned(),
        Value::Set(_) => "[object Set]".to_owned(),
        Value::Date(date) => {
            crate::date::iso_string(date.time_ms).unwrap_or_else(|| "Invalid Date".to_owned())
        }
        Value::ToolRef(_) => value.to_string(),
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
    if value == 0.0 {
        return "0".to_owned();
    }

    // Rust's shortest round-trip exponential form supplies the digits; the
    // fixed/exponential switch below follows ECMAScript Number::toString.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("exponential formatting always contains an exponent");
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits = mantissa
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect::<String>();

    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if (-6..=20).contains(&exponent) {
        if exponent >= 0 {
            let point = exponent as usize + 1;
            if digits.len() <= point {
                output.push_str(&digits);
                output.push_str(&"0".repeat(point - digits.len()));
            } else {
                output.push_str(&digits[..point]);
                output.push('.');
                output.push_str(&digits[point..]);
            }
        } else {
            output.push_str("0.");
            output.push_str(&"0".repeat((-exponent - 1) as usize));
            output.push_str(&digits);
        }
    } else {
        output.push_str(&digits[..1]);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        output.push('e');
        if exponent >= 0 {
            output.push('+');
        }
        output.push_str(&exponent.to_string());
    }
    output
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
        Value::Array(_)
        | Value::Object(_)
        | Value::Error(_)
        | Value::Promise(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Date(_) => "object",
        Value::ToolRef(path) if path.is_empty() => "object",
        Value::Function(_) | Value::Builtin(_) | Value::BoundMethod { .. } | Value::ToolRef(_) => {
            "function"
        }
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

    use super::{execute, Interpreter, RuntimeError};
    use crate::limits::ExecutionLimits;
    use crate::tools::{ToolError, ToolHost};
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
        let error = run("const Date = 1; return new Date();").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::UnsupportedSyntax);
    }

    #[test]
    fn rejects_unsupported_unary_operators() {
        let error = run("return void 0;").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::UnsupportedSyntax);
    }

    #[test]
    fn evaluates_stdlib_json_and_array_methods() {
        let source = r#"
            const data = JSON.parse('{"a":[1,2,3]}');
            const doubled = data.a.map((value) => value * 2);
            const summary = {
                total: doubled.reduce((sum, value) => sum + value, 0),
                max: Math.max(...doubled),
                keys: Object.keys(data),
            };
            return JSON.stringify(summary);
        "#;
        assert_eq!(
            run_value(source),
            string(r#"{"total":12,"max":6,"keys":["a"]}"#)
        );
    }

    #[test]
    fn evaluates_object_entries_and_string_methods() {
        let source = r#"
            const entries = Object.entries({ b: 2, a: 1 });
            const labels = entries.map(([key, value]) => `${key}=${value}`);
            const merged = Object.assign({}, { x: 1 }, { y: 2 });
            return {
                labels: labels.join("&"),
                keys: Object.keys(merged).join(","),
                split: "a,b,c".split(",").slice(1).join("-"),
                padded: "7".padStart(3, "0"),
                upper: " mixed Case ".trim().toUpperCase(),
                found: "banana".indexOf("na", 3),
                last: "banana".lastIndexOf("na"),
            };
        "#;
        let value = run_value(source);
        let object = value.as_object().expect("object result");
        assert_eq!(object.get("labels"), Some(&string("b=2&a=1")));
        assert_eq!(object.get("keys"), Some(&string("x,y")));
        assert_eq!(object.get("split"), Some(&string("b-c")));
        assert_eq!(object.get("padded"), Some(&string("007")));
        assert_eq!(object.get("upper"), Some(&string("MIXED CASE")));
        assert_eq!(object.get("found"), Some(&number(4.0)));
        assert_eq!(object.get("last"), Some(&number(4.0)));
    }

    #[test]
    fn evaluates_coercion_globals() {
        let source = r#"
            return [
                Number("0x10"),
                parseInt("  -42px", 10),
                parseFloat("3.5abc"),
                String(12),
                Boolean(""),
                isNaN("nope"),
                (255).toString(16),
                (1.005).toFixed(2),
            ];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], number(16.0));
        assert_eq!(array[1], number(-42.0));
        assert_eq!(array[2], number(3.5));
        assert_eq!(array[3], string("12"));
        assert_eq!(array[4], Value::Boolean(false));
        assert_eq!(array[5], Value::Boolean(true));
        assert_eq!(array[6], string("ff"));
        assert_eq!(array[7], string("1.00"));
    }

    #[test]
    fn evaluates_array_mutation_methods() {
        let source = r#"
            let values = [3, 1, 2];
            values.push(4);
            const last = values.pop();
            values = values.sort((a, b) => a - b);
            const removed = values.shift();
            values.unshift(9);
            return [values.join(","), last, removed, values.length];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("9,2,3"));
        assert_eq!(array[1], number(4.0));
        assert_eq!(array[2], number(1.0));
        assert_eq!(array[3], number(3.0));
    }

    #[test]
    fn binds_stdlib_methods_as_values() {
        let source = r#"
            const map = [1, 2].map;
            const keys = Object.keys;
            return [typeof map, typeof keys, map.length];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("function"));
        assert_eq!(array[1], string("function"));
        assert_eq!(array[2], Value::Undefined);
    }

    #[test]
    fn captures_console_output() {
        let allocator = Allocator::default();
        let program = parse_program(
            &allocator,
            r#"console.log("count", 2, true); console.warn("done");"#,
        )
        .expect("fixture should parse");
        let mut interpreter = Interpreter::new();
        let value = interpreter.run(&program).expect("program should run");
        assert_eq!(value, Value::Undefined);
        assert_eq!(interpreter.logs(), ["count 2 true", "[warn] done"]);
    }

    struct TestHost;

    impl ToolHost for TestHost {
        fn keys(&self, path: &[String]) -> Vec<String> {
            match path.join(".").as_str() {
                "" => vec![
                    "echo".to_owned(),
                    "fail".to_owned(),
                    "math".to_owned(),
                    "unsafe".to_owned(),
                ],
                "math" => vec!["add".to_owned()],
                _ => Vec::new(),
            }
        }

        fn invoke(
            &mut self,
            path: &[String],
            arguments: Vec<serde_json::Value>,
        ) -> Result<serde_json::Value, ToolError> {
            match path.join(".").as_str() {
                "math.add" => {
                    let left = arguments.first().and_then(serde_json::Value::as_f64);
                    let right = arguments.get(1).and_then(serde_json::Value::as_f64);
                    Ok(serde_json::json!(
                        left.unwrap_or(0.0) + right.unwrap_or(0.0)
                    ))
                }
                "echo" => Ok(arguments
                    .first()
                    .cloned()
                    .unwrap_or(serde_json::Value::Null)),
                "fail" => Err(ToolError::with_data(
                    "boom",
                    serde_json::json!({ "code": 42 }),
                )),
                "unsafe" => Ok(serde_json::json!({ "__proto__": { "polluted": true } })),
                _ => Err(ToolError::new(format!(
                    "Unknown tool '{}'.",
                    path.join(".")
                ))),
            }
        }
    }

    fn run_with_host(source: &str) -> Result<Value, RuntimeError> {
        let allocator = Allocator::default();
        let program = parse_program(&allocator, source).expect("fixture should parse");
        let mut interpreter = Interpreter::new();
        interpreter.set_tool_host(Box::new(TestHost));
        interpreter.run(&program)
    }

    #[test]
    fn invokes_tools_and_awaits_results() {
        assert_eq!(
            run_with_host("return await tools.math.add(2, 3);").expect("program should run"),
            number(5.0)
        );
    }

    #[test]
    fn exposes_tool_namespaces() {
        let source = r#"
            return [typeof tools, typeof tools.math, Object.keys(tools).join(","), Object.keys(tools.math).join(",")];
        "#;
        let value = run_with_host(source).expect("program should run");
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("object"));
        assert_eq!(array[1], string("function"));
        assert_eq!(array[2], string("echo,fail,math,unsafe"));
        assert_eq!(array[3], string("add"));
    }

    #[test]
    fn catches_tool_failures_with_structured_data() {
        let source = r#"
            try {
                await tools.fail();
            } catch (error) {
                return [error.message, error.data.code];
            }
            return ["no error", 0];
        "#;
        let value = run_with_host(source).expect("program should run");
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("boom"));
        assert_eq!(array[1], number(42.0));
    }

    #[test]
    fn rejects_non_data_tool_arguments() {
        let error = run_with_host("return tools.echo(() => 1);").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
    }

    #[test]
    fn rejects_blocked_tool_results() {
        let error = run_with_host("return tools.unsafe();").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
    }

    #[test]
    fn rejects_calling_the_tools_root() {
        let error = run_with_host("return tools();").expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
    }

    #[test]
    fn enforces_the_tool_call_budget() {
        let allocator = Allocator::default();
        let program = parse_program(&allocator, "tools.echo(1); return tools.echo(2);")
            .expect("fixture should parse");
        let mut interpreter = Interpreter::new();
        interpreter.set_tool_host(Box::new(TestHost));
        interpreter.set_tool_call_limit(Some(1));
        let error = interpreter.run(&program).expect_err("should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::ToolCallLimitExceeded);
        assert_eq!(interpreter.tool_calls(), ["echo"]);
    }

    #[test]
    fn evaluates_parity_globals_and_number_statics() {
        let source = r#"
            return [
                Number.isNaN(NaN),
                Number.isFinite(Infinity),
                Number.isInteger(3),
                Number.isSafeInteger(2 ** 53),
                String.fromCharCode(65, 66),
                undefined === undefined,
                Infinity > 1e9,
            ];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], Value::Boolean(true));
        assert_eq!(array[1], Value::Boolean(false));
        assert_eq!(array[2], Value::Boolean(true));
        assert_eq!(array[3], Value::Boolean(false));
        assert_eq!(array[4], string("AB"));
        assert_eq!(array[5], Value::Boolean(true));
        assert_eq!(array[6], Value::Boolean(true));
    }

    #[test]
    fn evaluates_parity_array_methods() {
        let source = r#"
            const values = [1, 2, 3, 4];
            const removed = values.splice(1, 2);
            values.fill(0, 0, 1);
            const copied = [1, 2, 3, 4, 5].copyWithin(0, 3);
            return [
                removed.join(","),
                values.join(","),
                copied.join(","),
                [10, 20].keys().join(","),
                [7, 8].values().join(","),
                JSON.stringify([7].entries()),
            ];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("2,3"));
        assert_eq!(array[1], string("0,4"));
        assert_eq!(array[2], string("4,5,3,4,5"));
        assert_eq!(array[3], string("0,1"));
        assert_eq!(array[4], string("7,8"));
        assert_eq!(array[5], string("[[0,7]]"));
    }

    #[test]
    fn coerces_objects_like_javascript() {
        let source = r#"
            let text = "a";
            text += { b: 1 };
            const other = "a" + [1, 2];
            return [text, other, String({}), `${[]}`];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("a[object Object]"));
        assert_eq!(array[1], string("a1,2"));
        assert_eq!(array[2], string("[object Object]"));
        assert_eq!(array[3], string(""));
    }

    #[test]
    fn evaluates_parity_string_methods() {
        assert_eq!(
            run_value(r#"return ["b", "a"].sort((x, y) => x.localeCompare(y)).join("");"#),
            string("ab")
        );
        assert_eq!(
            run_value(r#"return "  x ".trimLeft() + "|" + "  x ".trimRight();"#),
            string("x |  x")
        );
        assert_eq!(
            run_value(
                r#"return "banana".replace("na", (match, offset, input) => "[" + offset + "|" + input + "]");"#
            ),
            string("ba[2|banana]na")
        );
        assert_eq!(
            run_value(r#"return "ab".replaceAll("", (match, offset) => offset);"#),
            string("0a1b2")
        );
        assert_eq!(
            run_value(r#"return "aaa".replaceAll("aa", "-");"#),
            string("-a")
        );
        assert_eq!(
            run_value(r#"return "a1".replace("1", (match) => match === "1" ? 7 : 0);"#),
            string("a7")
        );
        assert_eq!(
            run_value(r#"return "a1".replace("1", () => null);"#),
            string("anull")
        );
        assert_eq!(
            run_value(r#"return "a1".replace("1", () => ({ n: 3 }));"#),
            string("a[object Object]")
        );
        let error = run("return 'a'.replace(1, 'b');").expect_err("non-string search");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
    }

    #[test]
    fn matches_upstream_sandbox_value_parity() {
        assert_eq!(
            run_value("const o = null; return JSON.stringify({ ...o, a: 1 });"),
            string("{\"a\":1}")
        );
        let error = run("return { ...[1, 2], a: 1 };").expect_err("array spread into object");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert_eq!(
            run_value("const e = new Error('m'); return ({ ...e }) instanceof Error;"),
            Value::Boolean(false)
        );
        assert_eq!(
            run_value("const e = new Error('m'); return JSON.stringify({ ...e });"),
            string("{\"name\":\"Error\",\"message\":\"m\"}")
        );
        assert_eq!(
            run_value(
                "const a = [1,2,3,4]; const r = a.splice(1,2); return JSON.stringify([r, a]);"
            ),
            string("[[2,3],[1,4]]")
        );
        assert_eq!(
            run_value("return JSON.stringify([1,2,3].fill('z'));"),
            string("[\"z\",\"z\",\"z\"]")
        );
        assert_eq!(
            run_value("return JSON.stringify([1,2,3,4,5].copyWithin(0,3));"),
            string("[4,5,3,4,5]")
        );
        assert_eq!(
            run_value("return [...['x','y','z'].keys()].join(',');"),
            string("0,1,2")
        );
        assert_eq!(
            run_value("return ['x','y'].values().join(',');"),
            string("x,y")
        );
        assert_eq!(
            run_value("return [...[7].entries()].map(([i, v]) => i + ':' + v).join(',');"),
            string("0:7")
        );
        assert_eq!(
            run_value("let d = new Date(1000); d += 1; return d;"),
            string("1970-01-01T00:00:01.000Z1")
        );
        assert_eq!(
            run_value("let d = new Date(1000); d -= 400; return d;"),
            number(600.0)
        );
        assert_eq!(
            run_value("let d = new Date(1000); d /= 4; return d;"),
            number(250.0)
        );
        assert_eq!(
            run_value("let x = 'a'; x += { b: 1 }; return x;"),
            string("a[object Object]")
        );
        assert_eq!(
            run_value("const o = { s: 't' }; o.s += new Date(0); return o.s;"),
            string("t1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value("let x = -7; x >>>= 1; return x;"),
            number(2_147_483_644.0)
        );
        assert_eq!(
            run_value("return [0, 1, '', 2, null, 3].filter(Boolean).join(',');"),
            string("1,2,3")
        );
        assert_eq!(
            run_value("return [1, 2, 3].map(String).join(',');"),
            string("1,2,3")
        );
        let error = run("return [1,2,3].map(42);").expect_err("non-callable callback");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
    }

    #[test]
    fn matches_reference_edge_cases_found_by_the_corpus() {
        assert_eq!(run_value("return 'abc'.lastIndexOf('', -1);"), number(0.0));
        assert_eq!(
            run_value("return 'abc'.lastIndexOf('', undefined);"),
            number(3.0)
        );
        assert_eq!(
            run_value("return JSON.stringify(Array.from(5));"),
            string("[]")
        );
        assert_eq!(
            run_value("return JSON.stringify(Array.from(undefined));"),
            string("[]")
        );
        assert_eq!(
            run_value("return String(Number.MIN_VALUE);"),
            string("5e-324")
        );
        assert_eq!(
            run_value("return Math.round(0.49999999999999994);"),
            number(0.0)
        );
        assert_eq!(
            run_value("return new Date('2024-01-02T24:00:00Z').toISOString();"),
            string("2024-01-03T00:00:00.000Z")
        );
        assert_eq!(
            run_value("return String(new Date('2024-01-02T24:00:01Z'));"),
            string("Invalid Date")
        );
        assert_eq!(
            run_value("return 'a,b'.split(',', -1).length;"),
            number(2.0)
        );
        assert_eq!(
            run_value("return 'a,b'.split(',', Infinity).length;"),
            number(0.0)
        );
        assert_eq!(
            run_value("return String(Math.pow(1, Infinity));"),
            string("NaN")
        );
        assert_eq!(
            run_value("return String(Math.pow(-Infinity, 3));"),
            string("-Infinity")
        );
        assert_eq!(
            run_value("return 'ab'.replaceAll('', (m, i) => i);"),
            string("0a1b2")
        );
        assert_eq!(
            run_value("return (1e-7).toPrecision(17);"),
            string("9.9999999999999995e-8")
        );
        assert_eq!(
            run_value("return (1e-7).toPrecision(21);"),
            string("9.99999999999999954748e-8")
        );
        assert_eq!(
            run_value("return (1e-7).toExponential(20);"),
            string("9.99999999999999954748e-8")
        );
        // String search positions are absolute; negative values clamp to 0.
        assert_eq!(run_value("return 'abcabc'.indexOf('a', -1);"), number(0.0));
        assert_eq!(
            run_value("return 'abcabc'.includes('a', -1);"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return 'abcabc'.startsWith('a', -1);"),
            Value::Boolean(true)
        );
        // Empty arrays must not underflow the lastIndex search window.
        assert_eq!(run_value("return [].lastIndexOf(1);"), number(-1.0));
        assert_eq!(
            run_value("return [].lastIndexOf(1, -Infinity);"),
            number(-1.0)
        );
    }

    #[test]
    fn parses_overflowing_json_numbers_as_infinity() {
        assert_eq!(
            run_value("return String(JSON.parse('1e400'));"),
            string("Infinity")
        );
        assert_eq!(
            run_value("return String(JSON.parse('-1e400'));"),
            string("-Infinity")
        );
        assert_eq!(
            run_value("return String(JSON.parse('{\"a\":1e400}').a);"),
            string("Infinity")
        );
        assert_eq!(
            run_value("return JSON.stringify(JSON.parse('[1e400,-1e400]'));"),
            string("[null,null]")
        );
        assert_eq!(
            run_value("return JSON.parse('\"1e400\"');"),
            string("1e400")
        );
        assert_eq!(
            run_value("return JSON.parse('{\"1e400\":1}')['1e400'];"),
            number(1.0)
        );
        assert_eq!(run_value("return JSON.parse('0e999');"), number(0.0));
        assert_eq!(
            run_value("return String(JSON.parse('1.7976931348623157e308'));"),
            string("1.7976931348623157e+308")
        );
    }

    #[test]
    fn supports_uri_encoding_helpers() {
        assert_eq!(
            run_value("return encodeURIComponent('a b&c');"),
            string("a%20b%26c")
        );
        assert_eq!(
            run_value("return encodeURI('http://a/b?c=d&e#f');"),
            string("http://a/b?c=d&e#f")
        );
        assert_eq!(
            run_value("return encodeURIComponent('é😀');"),
            string("%C3%A9%F0%9F%98%80")
        );
        assert_eq!(
            run_value("return decodeURIComponent('%C3%A9%F0%9F%98%80');"),
            string("é😀")
        );
        assert_eq!(run_value("return decodeURI('%3F%23');"), string("%3F%23"));
        assert_eq!(
            run_value("return decodeURIComponent('%3F%23');"),
            string("?#")
        );
        assert_eq!(
            run_value(
                "return [typeof encodeURIComponent, encodeURIComponent(undefined), \
                 encodeURIComponent(null)].join(',');"
            ),
            string("function,undefined,null")
        );
        assert_eq!(
            run_value("return ['a b'].map(encodeURIComponent).join(',');"),
            string("a%20b")
        );
        assert_eq!(
            run_value(
                "try { decodeURIComponent('%'); } catch (error) { \
                 return [error.name, error.message].join('|'); }"
            ),
            string("URIError|decodeURIComponent received malformed URI data: URI malformed")
        );
        assert_eq!(
            run_value("return [new URIError('x') instanceof URIError, new EvalError('y') instanceof EvalError].join(',');"),
            string("true,true")
        );

        let error = run("return encodeURIComponent(() => 1);").expect_err("callable URI input");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert!(error
            .message()
            .contains("encodeURIComponent input must contain data only"));
        let error = run("return decodeURI(Promise.resolve(1));").expect_err("promise URI input");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert!(error.message().contains("un-awaited Promise"));
    }

    #[test]
    fn array_reverse_and_sort_do_not_mutate_like_upstream() {
        assert_eq!(
            run_value(
                "const a = [1, 2, 3]; const r = a.reverse(); \
                 return [a.join(','), r.join(',')].join('|');"
            ),
            string("1,2,3|3,2,1")
        );
        assert_eq!(
            run_value(
                "const a = [3, 1, 2]; const r = a.sort(); \
                 return [a.join(','), r.join(',')].join('|');"
            ),
            string("3,1,2|1,2,3")
        );
        assert_eq!(
            run_value(
                "const a = [3, 1, 2]; const r = a.sort((x, y) => x - y); \
                 return [a.join(','), r.join(',')].join('|');"
            ),
            string("3,1,2|1,2,3")
        );
        assert_eq!(
            run_value(
                "const a = [1, 2]; const r = a.toSorted(); \
                 return [a.join(','), r.join(',')].join('|');"
            ),
            string("1,2|1,2")
        );
    }

    #[test]
    fn coercion_builtins_reject_opaque_values() {
        for (source, message) in [
            (
                "const f = () => 1; return String(f);",
                "String input must contain data only",
            ),
            (
                "const f = () => 1; return Number(f);",
                "Number input must contain data only",
            ),
            (
                "const f = () => 1; return Boolean(f);",
                "Boolean input must contain data only",
            ),
            (
                "const f = () => 1; return parseInt(f);",
                "parseInt input must contain data only",
            ),
            (
                "const f = () => 1; return parseFloat(f);",
                "parseFloat input must contain data only",
            ),
            (
                "return String({ f: () => 1 });",
                "String input must contain data only",
            ),
            ("return Boolean(Promise.resolve(1));", "un-awaited Promise"),
        ] {
            let error = run(source).expect_err("opaque coercion input");
            assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
            assert!(error.message().contains(message), "{}", error.message());
        }
        assert_eq!(
            run_value("return String(new Map());"),
            string("[object Map]")
        );
        assert_eq!(
            run_value("return String(new Set());"),
            string("[object Set]")
        );
        assert_eq!(
            run_value("return String(new Date(0));"),
            string("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value("return String(new Error('m'));"),
            string("[object Object]")
        );
        assert_eq!(run_value("return Number(new Date(5));"), number(5.0));
        assert_eq!(
            run_value("return Boolean(new Map());"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn matches_upstream_stdlib_collection_and_date_parity() {
        assert_eq!(
            run_value("return new Date('2024-01-02T03:04:05.000Z').getTime();"),
            number(1_704_164_645_000.0)
        );
        assert_eq!(run_value("return +new Date(42);"), number(42.0));
        assert_eq!(
            run_value("const d = new Date(2024, 0, 2); return [d.getFullYear(), d.getMonth(), d.getDate()].join(',');"),
            string("2024,0,2")
        );
        assert_eq!(
            run_value("return new Date('garbage').toJSON();"),
            Value::Null
        );
        assert_eq!(
            run_value("return String(new Date(0));"),
            string("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(run_value("return Number(new Date(5));"), number(5.0));
        assert_eq!(
            run_value("return [Boolean(new Date(0)), Boolean(new Map())].join(',');"),
            string("true,true")
        );
        assert_eq!(
            run_value(
                "const dates = [new Date(3000), new Date(1000), new Date(2000)]; \
                 return dates.sort((a, b) => a - b).map((d) => d.getTime()).join(',');"
            ),
            string("1000,2000,3000")
        );

        assert_eq!(
            run_value(
                "const m = new Map(); m.set('a', 1).set('b', 2); \
                 return [m.get('a'), m.has('a'), m.get('zz') === undefined, m.size].join(',');"
            ),
            string("1,true,true,2")
        );
        assert_eq!(
            run_value(
                "const key = { id: 1 }; const m = new Map(); m.set(key, 'hit'); \
                 return [m.get(key), m.get({ id: 1 }) === undefined].join(',');"
            ),
            string("hit,true")
        );
        assert_eq!(
            run_value(
                "const m = new Map([['a', 1], ['b', 2]]); \
                 return JSON.stringify([m.keys(), m.values(), m.entries()]);"
            ),
            string("[[\"a\",\"b\"],[1,2],[[\"a\",1],[\"b\",2]]]")
        );
        assert_eq!(
            run_value("return JSON.stringify(Array.from(new Map([['a', 1]])));"),
            string("[[\"a\",1]]")
        );
        assert_eq!(
            run_value(
                "const m = new Map([['a', 1], ['b', 2]]); const seen = []; \
                 m.forEach((count, key) => seen.push(key + count)); return seen.join(',');"
            ),
            string("a1,b2")
        );
        assert_eq!(
            run_value(
                "const m = new Map([['a', 1], ['b', 2]]); const removed = m.delete('a'); \
                 const missed = m.delete('zz'); const sizeAfterDelete = m.size; m.clear(); \
                 return [removed, missed, sizeAfterDelete, m.size].join(',');"
            ),
            string("true,false,1,0")
        );
        let error = run("return new Map('nope');").expect_err("string map initializer");
        assert!(error.message().contains("[key, value] pairs"));
        let error = run("return new Map(['flat']);").expect_err("flat pair initializer");
        assert!(error.message().contains("[key, value] pairs"));

        assert_eq!(
            run_value(
                "const s = new Set(); s.add(1).add(2).add(1); const removed = s.delete(2); \
                 return [s.size, s.has(1), s.has(2), removed].join(',');"
            ),
            string("1,true,false,true")
        );
        assert_eq!(
            run_value("return [...new Set('aba')].join('');"),
            string("ab")
        );
        assert_eq!(
            run_value("return new Set([NaN]).has(NaN);"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(
                "const m = new Map([['start', new Date(1000)]]); return m.get('start').getTime();"
            ),
            number(1000.0)
        );
        assert_eq!(
            run_value("return JSON.stringify({ ...new Map([['a', 1]]), kept: true });"),
            string("{\"kept\":true}")
        );

        assert_eq!(
            run_value("return Object.values({ d: new Date(0) })[0].getTime();"),
            number(0.0)
        );
        assert_eq!(
            run_value(
                "const [key, d] = Object.entries({ d: new Date(0) })[0]; \
                 return key + ':' + d.getTime();"
            ),
            string("d:0")
        );
        assert_eq!(
            run_value(
                "const merged = Object.assign({}, { m: new Map([['a', 1]]) }); \
                 return merged.m.get('a');"
            ),
            number(1.0)
        );
        assert_eq!(
            run_value(
                "const src = { m: new Map([['a', 1]]) }; const copy = { ...src }; \
                 copy.m.set('b', 2); return [copy.m.get('a'), src.m.get('b')].join(',');"
            ),
            string("1,2")
        );
        assert_eq!(
            run_value(
                "const list = [new Date(1000)]; const copy = [...list]; return copy[0].getTime();"
            ),
            number(1000.0)
        );
        assert_eq!(
            run_value("return Array.from([new Date(5)])[0].getTime();"),
            number(5.0)
        );
        assert_eq!(
            run_value(
                "return [Object.keys(new Map([['a', 1]])).length, \
                 Object.values(new Date(0)).length, \
                 Object.entries(new Set([1])).length].join(',');"
            ),
            string("0,0,0")
        );
        assert_eq!(
            run_value("return JSON.stringify(Object.assign({}, new Map([['a', 1]])));"),
            string("{}")
        );
        assert_eq!(
            run_value("return Object.hasOwn(new Date(0), 'time');"),
            Value::Boolean(false)
        );
        let error = run("return Object.assign({}, Promise.resolve(1));")
            .expect_err("un-awaited promise source");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert!(error.message().contains("un-awaited Promise"));
    }

    #[test]
    fn matches_upstream_instanceof_and_namespace_types() {
        assert_eq!(
            run_value(
                "return [typeof Object, typeof Array, typeof Promise, typeof Math, typeof JSON].join(',');"
            ),
            string("function,function,function,object,object")
        );
        assert_eq!(
            run_value(
                "return [[1] instanceof Array, [] instanceof Object, null instanceof Object, \
                 1 instanceof Object, Promise.resolve(1) instanceof Promise, \
                 new Date(0) instanceof Date, new Map() instanceof Map, \
                 new Set() instanceof Set, new Error('x') instanceof Error].join(',');"
            ),
            string("true,true,false,false,true,true,true,true,true")
        );
        assert_eq!(
            run_value("return [1 instanceof Number, 'x' instanceof String, true instanceof Boolean].join(',');"),
            string("false,false,false")
        );
        let error = run("return 1 instanceof 5;").expect_err("unknown constructor");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        assert!(error.message().contains("right-hand side of 'instanceof'"));
        let error = run("return 1 instanceof {};").expect_err("plain object constructor");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
    }

    #[test]
    fn rejects_opaque_operands_and_unawaited_promises() {
        for source in [
            "return Promise.resolve(1) + 1;",
            "return 1 + Promise.resolve(1);",
            "const f = () => 1; return f + 1;",
            "const f = () => 1; return [f] + '';",
            "return tools + '';",
        ] {
            let error = run(source).expect_err("opaque operands must be rejected");
            assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
            assert!(error
                .message()
                .contains("Binary operators require data values"));
        }
        let error = run("return JSON.stringify(Promise.resolve(1));")
            .expect_err("un-awaited promise serialization");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert!(error.message().contains("un-awaited Promise"));
        let error = run("return JSON.stringify({ p: Promise.resolve(1) });")
            .expect_err("nested un-awaited promise serialization");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert!(error.message().contains("un-awaited Promise"));

        assert_eq!(
            run_value("return typeof Promise.resolve(1);"),
            string("object")
        );
        assert_eq!(
            run_value("return Promise.resolve(1) instanceof Promise;"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return JSON.stringify(new Map([['a', 1]]));"),
            string("{}")
        );
        assert_eq!(
            run_value("return JSON.stringify(new Error('m'), null, 2);"),
            string("{\n  \"name\": \"Error\",\n  \"message\": \"m\"\n}")
        );
        let error = run("const f = () => 1; return JSON.stringify(f);")
            .expect_err("functions are not data");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert!(error.message().contains("data only"));
    }

    #[test]
    fn for_in_rejects_non_enumerable_values() {
        for expression in ["'text'", "new Map([[1, 2]])", "new Set([1])", "42", "null"] {
            let source = format!("for (const key in {expression}) {{}}; return 'no';");
            let error = run(&source).expect_err("for...in must reject the value");
            assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
            assert!(error
                .message()
                .contains("for...in requires a plain object, array, or tools reference"));
            assert!(error
                .message()
                .contains("Use for...of for arrays/strings/Maps/Sets"));
        }
    }

    #[test]
    fn for_in_enumerates_error_properties() {
        assert_eq!(
            run_value(
                "const out = []; for (const key in new Error('m')) out.push(key); return JSON.stringify(out);"
            ),
            string("[\"name\",\"message\"]")
        );
    }

    #[test]
    fn enumerates_error_properties_like_upstream_data_objects() {
        assert_eq!(
            run_value("return JSON.stringify(Object.keys(new Error('m')));"),
            string("[\"name\",\"message\"]")
        );
        assert_eq!(
            run_value("return JSON.stringify(Object.values(new Error('m')));"),
            string("[\"Error\",\"m\"]")
        );
        assert_eq!(
            run_value("return JSON.stringify(Object.entries(new TypeError('m')));"),
            string("[[\"name\",\"TypeError\"],[\"message\",\"m\"]]")
        );
        assert_eq!(
            run_value("return JSON.stringify(Object.assign({}, new Error('m')));"),
            string("{\"name\":\"Error\",\"message\":\"m\"}")
        );
        assert_eq!(
            run_value("return JSON.stringify({...new Error('m')});"),
            string("{\"name\":\"Error\",\"message\":\"m\"}")
        );
    }

    #[test]
    fn rejects_circular_splice_insertion() {
        let error = run("const a = [1]; a.splice(0, 0, [a]); return a;")
            .expect_err("should reject a circular insertion");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
    }

    #[test]
    fn rejects_circular_results() {
        for source in [
            "const a = []; a.push(a); return a;",
            "const a = []; a.unshift(a); return a;",
            "const a = []; a.fill(a); return a;",
            "const a = []; a.push([a]); return a;",
            "const a = []; a.splice(0, 0, { nested: a }); return a;",
        ] {
            let error = run(source).expect_err("should reject a circular insertion");
            assert_eq!(
                error.kind(),
                super::RuntimeErrorKind::InvalidDataValue,
                "{source}"
            );
        }
    }

    #[test]
    fn constructs_and_brands_errors() {
        let source = r#"
            const generic = new Error("boom");
            const type = new TypeError("t");
            return [
                generic instanceof Error,
                type instanceof Error,
                type instanceof TypeError,
                type instanceof RangeError,
                generic instanceof TypeError,
                generic.name,
                generic.message,
                typeof Error,
            ];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], Value::Boolean(true));
        assert_eq!(array[1], Value::Boolean(true));
        assert_eq!(array[2], Value::Boolean(true));
        assert_eq!(array[3], Value::Boolean(false));
        assert_eq!(array[4], Value::Boolean(false));
        assert_eq!(array[5], string("Error"));
        assert_eq!(array[6], string("boom"));
        assert_eq!(array[7], string("function"));
    }

    #[test]
    fn errors_constructed_without_new_and_empty_messages() {
        let source = r#"
            const called = Error("plain");
            const empty = new Error();
            return [called instanceof Error, called.name, called.message, empty.name, empty.message];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], Value::Boolean(true));
        assert_eq!(array[1], string("Error"));
        assert_eq!(array[2], string("plain"));
        assert_eq!(array[3], string("Error"));
        assert_eq!(array[4], string(""));
    }

    #[test]
    fn brands_runtime_failures_as_caught_errors() {
        let syntax = run_value(
            r#"try { JSON.parse("nope") } catch (e) { return [e.name, e instanceof SyntaxError, e instanceof Error, e.message.includes("JSON")] }"#,
        );
        let syntax = syntax.as_array().expect("array result");
        assert_eq!(syntax[0], string("SyntaxError"));
        assert_eq!(syntax[1], Value::Boolean(true));
        assert_eq!(syntax[2], Value::Boolean(true));
        assert_eq!(syntax[3], Value::Boolean(true));

        let reference = run_value(
            r#"try { undeclared() } catch (e) { return [e.name, e instanceof ReferenceError] }"#,
        );
        let reference = reference.as_array().expect("array result");
        assert_eq!(reference[0], string("ReferenceError"));
        assert_eq!(reference[1], Value::Boolean(true));

        let type_error = run_value(
            r#"try { const c = 1; c = 2 } catch (e) { return [e.name, e instanceof TypeError] }"#,
        );
        let type_error = type_error.as_array().expect("array result");
        assert_eq!(type_error[0], string("TypeError"));
        assert_eq!(type_error[1], Value::Boolean(true));

        let plain =
            run_value(r#"try { JSON.parse(5) } catch (e) { return [e.name, e instanceof Error] }"#);
        let plain = plain.as_array().expect("array result");
        assert_eq!(plain[0], string("Error"));
        assert_eq!(plain[1], Value::Boolean(true));
    }

    #[test]
    fn errors_serialize_as_plain_data_and_lose_the_brand() {
        assert_eq!(
            run_value(r#"return JSON.stringify(new Error("m"));"#),
            string(r#"{"name":"Error","message":"m"}"#)
        );
        assert_eq!(
            run_value(r#"return Object.keys(new Error("m")).join(",");"#),
            string("name,message")
        );
        assert_eq!(
            run_value(r#"return ({ ...new Error("m") }) instanceof Error;"#),
            Value::Boolean(false)
        );
        assert_eq!(
            run_value(r#"return JSON.stringify({ ...new Error("m") });"#),
            string(r#"{"name":"Error","message":"m"}"#)
        );
    }

    #[test]
    fn thrown_errors_keep_their_brand() {
        let value = run_value(
            r#"try { throw new Error("x") } catch (e) { return [e instanceof Error, e.message] }"#,
        );
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], Value::Boolean(true));
        assert_eq!(array[1], string("x"));
    }

    #[test]
    fn rejects_invalid_instanceof_right_hand_sides() {
        let error = run("return 1 instanceof 5;").expect_err("should reject the operand");
        assert!(error.message().contains("right-hand side of 'instanceof'"));
    }

    #[test]
    fn promise_all_settled_keeps_rejection_reasons() {
        let source = r#"
            const settled = await Promise.allSettled([Promise.reject(new Error("b"))]);
            return [
                settled[0].status,
                settled[0].reason instanceof Error,
                settled[0].reason.message,
            ];
        "#;
        let value = run_value(source);
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("rejected"));
        assert_eq!(array[1], Value::Boolean(true));
        assert_eq!(array[2], string("b"));
    }

    #[test]
    fn promise_surface_hints_at_await() {
        let error = run("return Promise.resolve(1).then((x) => x);")
            .expect_err("then should be unsupported");
        assert_eq!(error.kind(), super::RuntimeErrorKind::UnsupportedSyntax);
        assert!(error
            .message()
            .contains("Promise.prototype.then is not supported"));
        assert!(error.message().contains("await"));

        let error =
            run("return Promise.resolve(1).value;").expect_err("property reads should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::InvalidDataValue);
        assert!(error.message().contains("un-awaited Promise"));
        assert!(error.message().contains("await it first"));

        let error = run("return await Promise.race([]);").expect_err("empty race should fail");
        assert!(error.message().contains("never settle"));
    }

    #[test]
    fn evaluates_maps_with_same_value_zero_keys() {
        assert_eq!(
            run_value(
                r#"const m = new Map([["a",1],["b",2]]); return [m.size, m.get("a"), m.has("b"), m.get("z") === undefined].join(",");"#
            ),
            string("2,1,true,true")
        );
        assert_eq!(
            run_value(r#"const m = new Map(); m.set(NaN, "nan"); return m.get(NaN);"#),
            string("nan")
        );
        assert_eq!(
            run_value(
                r#"const m = new Map(); const key = { id: 1 }; m.set(key, "yes"); return [m.get(key), m.get({ id: 1 }) === undefined].join(",");"#
            ),
            string("yes,true")
        );
        assert_eq!(
            run_value(
                r#"const m = new Map([["a",1],["b",2]]); const same = m.set("c", 3) === m; return [same, m.size, m.delete("a"), m.has("a")].join(",");"#
            ),
            string("true,3,true,false")
        );
        assert_eq!(
            run_value(
                r#"const source = new Map([["a",1]]); const copy = new Map(source); return copy.get("a");"#
            ),
            number(1.0)
        );
    }

    #[test]
    fn iterates_maps_in_insertion_order() {
        let value = run_value(
            r#"
            const m = new Map([["a",1],["b",2]]);
            const out = [];
            m.forEach((value, key, map) => out.push(key + ":" + value + ":" + (map === m)));
            for (const [key, value] of m) out.push(key + "=" + value);
            out.push([...m.keys()].join(","));
            out.push([...m.values()].join(","));
            out.push(JSON.stringify([...m.entries()]));
            return out.join("|");
        "#,
        );
        assert_eq!(
            value,
            string(r#"a:1:true|b:2:true|a=1|b=2|a,b|1,2|[["a",1],["b",2]]"#)
        );
    }

    #[test]
    fn evaluates_sets_with_same_value_zero_members() {
        assert_eq!(
            run_value(
                r#"const s = new Set([1,1,2,NaN,NaN]); return [s.size, s.has(1), s.has(NaN), s.has(4)].join(",");"#
            ),
            string("3,true,true,false")
        );
        assert_eq!(
            run_value(
                r#"const s = new Set([1]); const same = s.add(2) === s; return [same, [...s.values()].join(","), s.delete(1), [...s.values()].join(",")].join("|");"#
            ),
            string("true|1,2|true|2")
        );
        assert_eq!(
            run_value(r#"return [...new Set("aba")].join("");"#),
            string("ab")
        );
        assert_eq!(
            run_value(r#"const s = new Set([1]); return JSON.stringify([...s.entries()]);"#),
            string("[[1,1]]")
        );
    }

    #[test]
    fn iterates_sets_and_for_each_arguments() {
        let value = run_value(
            r#"
            const s = new Set(["x","y"]);
            const out = [];
            s.forEach((value, key, set) => out.push(key + ":" + value + ":" + (set === s)));
            for (const value of s) out.push(value);
            return out.join("|");
        "#,
        );
        assert_eq!(value, string("x:x:true|y:y:true|x|y"));
    }

    #[test]
    fn serializes_collections_as_empty_objects_at_the_boundary() {
        let value = run_value(
            r#"return [JSON.stringify(new Map([["a",1]])), JSON.stringify(new Set([1])), Object.keys(new Map([["a",1]])).length].join(",");"#,
        );
        assert_eq!(value, string("{},{},0"));
    }

    #[test]
    fn brands_maps_and_sets_for_instanceof() {
        let value = run_value(
            r#"return [new Map() instanceof Map, new Set() instanceof Set, ({}) instanceof Map, new Map() instanceof Set].join(",");"#,
        );
        assert_eq!(value, string("true,true,false,false"));
    }

    #[test]
    fn rejects_invalid_collection_construction() {
        let error = run("return new Map(5);").expect_err("should reject the argument");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        assert!(error.message().contains("[key, value] pairs"));

        let error = run("return new Map([1]);").expect_err("should reject the pair");
        assert!(error.message().contains("[key, value] pairs"));

        let error = run("return new Set(5);").expect_err("should reject the argument");
        assert!(error.message().contains("expects an array"));

        let error = run("return Map();").expect_err("should require new");
        assert!(error.message().contains("requires 'new'"));

        let error = run("return Set();").expect_err("should require new");
        assert!(error.message().contains("requires 'new'"));
    }

    #[test]
    fn evaluates_dates_and_iso_rendering() {
        assert_eq!(
            run_value("return new Date(0).toISOString();"),
            string("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value(r#"return new Date("2024-01-02T03:04:05.000Z").getTime();"#),
            number(1_704_164_645_000.0)
        );
        assert_eq!(
            run_value(r#"return Date.parse("2024-01-02T03:04:05.000Z");"#),
            number(1_704_164_645_000.0)
        );
        assert_eq!(
            run_value(r#"return Date.parse("2024-01-02T03:04:05+09:00");"#),
            number(1_704_132_245_000.0)
        );
        assert_eq!(
            run_value("const a = new Date(1000); const b = new Date(3000); return b - a;"),
            number(2_000.0)
        );
        assert_eq!(
            run_value("const a = new Date(1000); const b = new Date(3000); return a < b;"),
            Value::Boolean(true)
        );
        assert_eq!(run_value("return +new Date(42);"), number(42.0));
    }

    #[test]
    fn reads_date_components() {
        let value = run_value(
            r#"
            const d = new Date("2024-03-05T06:07:08.009Z");
            return [
                d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), d.getUTCDay(),
                d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds(),
                d.getUTCMilliseconds(), d.getTimezoneOffset(),
            ];
        "#,
        );
        let array = value.as_array().expect("array result");
        let expected = [2024.0, 2.0, 5.0, 2.0, 6.0, 7.0, 8.0, 9.0, 0.0];
        for (item, expected) in array.iter().zip(expected) {
            assert_eq!(*item, number(expected));
        }

        assert_eq!(
            run_value(
                r#"const d = new Date(2024, 0, 2); return [d.getFullYear(), d.getMonth(), d.getDate()].join(",");"#
            ),
            string("2024,0,2")
        );
        assert_eq!(
            run_value(r#"return Date.UTC(2024, 0, 2);"#),
            number(1_704_153_600_000.0)
        );
        assert_eq!(
            run_value(r#"return Number.isNaN(new Date(2024, NaN).getTime());"#),
            Value::Boolean(true)
        );
    }

    #[test]
    fn handles_invalid_dates() {
        assert_eq!(
            run_value(r#"return Number.isNaN(new Date("garbage").getTime());"#),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(r#"return new Date("garbage").toJSON();"#),
            Value::Null
        );
        let error = run(r#"return new Date("garbage").toISOString();"#)
            .expect_err("invalid ISO formatting should throw");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
        assert!(error.message().contains("Invalid time value."));
    }

    #[test]
    fn serializes_and_coerces_dates() {
        assert_eq!(
            run_value("return `at ${new Date(0)}`;"),
            string("at 1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value(r#"return JSON.stringify({ d: new Date(0) });"#),
            string(r#"{"d":"1970-01-01T00:00:00.000Z"}"#)
        );
        assert_eq!(run_value("return Number(new Date(5));"), number(5.0));
        assert_eq!(
            run_value("return String(new Date(0));"),
            string("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value("return Boolean(new Date(0));"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("let d = new Date(1000); d += 1; return d;"),
            string("1970-01-01T00:00:01.000Z1")
        );
        assert_eq!(
            run_value("return new Date(0) instanceof Date;"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn sorts_dates_and_reports_object_type() {
        let value = run_value(
            r#"
            const dates = [new Date(3000), new Date(1000), new Date(2000)];
            return dates.sort((a, b) => a - b).map((d) => d.getTime());
        "#,
        );
        let array = value.as_array().expect("array result");
        assert_eq!(
            array.as_slice(),
            &[number(1000.0), number(2000.0), number(3000.0)]
        );
        assert_eq!(run_value("return typeof new Date(0);"), string("object"));
        assert_eq!(
            run_value("return new Date(0).nope === undefined;"),
            Value::Boolean(true)
        );
        assert_eq!(run_value("return typeof Date.now();"), string("number"));
        assert_eq!(
            run_value("const t = Date.now(); return t > 0 && t < 1e15;"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn program_variables_shadow_installed_globals() {
        assert_eq!(run_value("const Date = 1; return Date;"), number(1.0));
        assert_eq!(run_value("let Map = 2; return Map;"), number(2.0));
    }

    #[test]
    fn supports_extended_array_methods() {
        assert_eq!(run_value("return [1,2,1].lastIndexOf(1);"), number(2.0));
        assert_eq!(run_value("return [1,2,1].lastIndexOf(1, 1);"), number(0.0));
        assert_eq!(
            run_value("return [1,2,1].lastIndexOf(1, undefined);"),
            number(2.0)
        );
        assert_eq!(run_value("return [1,2,1].lastIndexOf(1, -2);"), number(0.0));
        assert_eq!(run_value("return [1,2,3].lastIndexOf(4);"), number(-1.0));

        assert_eq!(
            run_value("return [1,2,3,4].findLast((x) => x % 2 === 0);"),
            number(4.0)
        );
        assert_eq!(
            run_value("return [1,2,3,4].findLastIndex((x) => x % 2 === 0);"),
            number(3.0)
        );
        assert_eq!(
            run_value("return [1,3].findLast((x) => x % 2 === 0) === undefined;"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return [1,3].findLastIndex((x) => x % 2 === 0);"),
            number(-1.0)
        );

        assert_eq!(
            run_value("return [1,2].flatMap((x) => [x, x * 2]).join(',');"),
            string("1,2,2,4")
        );
        assert_eq!(
            run_value("return [1,2].flatMap((x) => x * 2).join(',');"),
            string("2,4")
        );

        assert_eq!(
            run_value(
                "const a = [3,1,2]; const b = a.toSorted(); return [a.join(','), b.join(',')].join('|');"
            ),
            string("3,1,2|1,2,3")
        );
        assert_eq!(
            run_value("return [3,1,2].toSorted((a, b) => b - a).join(',');"),
            string("3,2,1")
        );
        assert_eq!(
            run_value(
                "const a = [1,2,3]; return [a.toReversed().join(','), a.join(',')].join('|');"
            ),
            string("3,2,1|1,2,3")
        );
        assert_eq!(
            run_value("return [1,2,3].with(-1, 9).join(',');"),
            string("1,2,9")
        );
        assert_eq!(
            run_value(
                "const a = [1,2,3]; const b = a.with(0, 9); return [a.join(','), b.join(',')].join('|');"
            ),
            string("1,2,3|9,2,3")
        );
        let error = run("return [1,2,3].with(3, 9);").expect_err("out of range");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
    }

    #[test]
    fn formats_console_values_like_a_debugger() {
        let allocator = Allocator::default();
        let program = parse_program(
            &allocator,
            r#"
            console.log({ m: new Map([["a", 1]]), when: new Date(0), s: new Set([1, 2]) });
            console.log([new Date(0)]);
            const cyclic = new Map();
            cyclic.set("self", cyclic);
            console.log({ box: cyclic });
            console.log({ fn: (x) => x, ok: 1 });
            console.debug("d");
            console.error("e");
            console.dir({ nested: [1] });
            console.table([{ when: new Date(0), n: NaN }]);
        "#,
        )
        .expect("fixture should parse");
        let mut interpreter = Interpreter::new();
        interpreter.run(&program).expect("program should run");
        assert_eq!(
            interpreter.logs(),
            [
                r#"{"m":Map(1) [["a",1]],"when":1970-01-01T00:00:00.000Z,"s":Set(2) [1,2]}"#,
                "[1970-01-01T00:00:00.000Z]",
                r#"{"box":Map(1) [["self",[Circular]]]}"#,
                r#"{"fn":[CodeMode reference],"ok":1}"#,
                "[debug] d",
                "[error] e",
                r#"{"nested":[1]}"#,
                "(index)\twhen\tn\n0\t1970-01-01T00:00:00.000Z\tNaN",
            ]
        );
    }

    #[test]
    fn supports_object_and_array_static_gaps() {
        assert_eq!(
            run_value(
                r#"const o = { a: 1 }; return [Object.hasOwn(o, "a"), Object.hasOwn(o, "b")].join(",");"#
            ),
            string("true,false")
        );
        let error = run("return Object.hasOwn([1, 2], 1);").expect_err("arrays are not accepted");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        assert_eq!(
            run_value(
                r#"const m = new Map([["a", 1], ["b", 2]]); return JSON.stringify(Object.fromEntries(m));"#
            ),
            string(r#"{"a":1,"b":2}"#)
        );
        assert_eq!(
            run_value(r#"return JSON.stringify(Object.fromEntries([[true, 1]]));"#),
            string(r#"{"true":1}"#)
        );
        assert_eq!(
            run_value("return Array.of(1, 2, 3).join(',');"),
            string("1,2,3")
        );
        assert_eq!(
            run_value(r#"return Array.from({ length: 3, 0: "a", 2: "c" }).join(",");"#),
            string("a,,c")
        );
        let error = run(r#"return Array.from({ length: 2, 0: 1, 1: 2 }, (x) => x + 1);"#)
            .expect_err("Array.from map functions are unsupported");
        assert_eq!(error.kind(), super::RuntimeErrorKind::UnsupportedSyntax);
        let error = run(r#"return Array.from({ length: 1000001 });"#)
            .expect_err("oversized array-like should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
    }

    #[test]
    fn supports_additional_string_and_math_methods() {
        assert_eq!(
            run_value(r#"return "a-b-a".replaceAll("a", "x");"#),
            string("x-b-x")
        );
        assert_eq!(
            run_value(r#"return "abc".replace("b", "[$&]");"#),
            string("a[b]c")
        );
        assert_eq!(
            run_value(r#"return "abc".replace("b", "$`");"#),
            string("aac")
        );
        assert_eq!(
            run_value(r#"return "abc".replace("b", "$'");"#),
            string("acc")
        );
        assert_eq!(
            run_value(r#"return "abc".replace("b", "$$");"#),
            string("a$c")
        );
        assert_eq!(
            run_value(r#"return "abc".replaceAll("", "-");"#),
            string("-a-b-c-")
        );
        assert_eq!(
            run_value(r#"return "😀".codePointAt(0);"#),
            number(128_512.0)
        );
        assert_eq!(
            run_value(r#"return "ab".codePointAt(5) === undefined;"#),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(r#"return ["ab".charAt(), "ab".charAt(NaN), "ab".charCodeAt()].join(",");"#),
            string("a,a,97")
        );
        assert_eq!(run_value(r#"return "abc".toString();"#), string("abc"));

        assert_eq!(run_value("return Math.cbrt(27);"), number(3.0));
        assert_eq!(run_value("return Math.log2(8);"), number(3.0));
        assert_eq!(run_value("return Math.log10(1000);"), number(3.0));
        assert_eq!(run_value("return Math.hypot(3, 4);"), number(5.0));
        assert_eq!(run_value("return Math.hypot();"), number(0.0));
        assert_eq!(
            run_value("return Math.hypot(Infinity, NaN);"),
            number(f64::INFINITY)
        );
        assert_eq!(
            run_value("return Math.SQRT2 === Math.sqrt(2);"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return Math.LN2 === Math.log(2);"),
            Value::Boolean(true)
        );
    }

    #[test]
    fn supports_number_precision_and_formatting() {
        assert_eq!(
            run_value("return (123.456).toPrecision(4);"),
            string("123.5")
        );
        assert_eq!(
            run_value("return (0.000001234).toPrecision(2);"),
            string("0.0000012")
        );
        assert_eq!(
            run_value("return (0.0000001234).toPrecision(2);"),
            string("1.2e-7")
        );
        assert_eq!(
            run_value("return (999.9).toPrecision(3);"),
            string("1.00e+3")
        );
        assert_eq!(run_value("return (9.99).toPrecision(2);"), string("10"));
        assert_eq!(run_value("return (1).toPrecision(3);"), string("1.00"));
        assert_eq!(run_value("return (100).toPrecision(2);"), string("1.0e+2"));
        assert_eq!(run_value("return (0).toPrecision(3);"), string("0.00"));
        assert_eq!(run_value("return (-1.234).toPrecision(2);"), string("-1.2"));

        assert_eq!(
            run_value("return (123.456).toExponential();"),
            string("1.23456e+2")
        );
        assert_eq!(
            run_value("return (123.456).toExponential(2);"),
            string("1.23e+2")
        );
        assert_eq!(run_value("return (0).toExponential();"), string("0e+0"));
        assert_eq!(run_value("return (0).toExponential(2);"), string("0.00e+0"));
        assert_eq!(run_value("return (1).toExponential(2);"), string("1.00e+0"));
        assert_eq!(run_value("return (1.005).toFixed(2);"), string("1.00"));
        assert_eq!(run_value("return (1e21).toFixed(2);"), string("1e+21"));
        assert_eq!(run_value("return (-0).toFixed(2);"), string("0.00"));
        assert_eq!(run_value("return (-0).toExponential();"), string("0e+0"));
        assert_eq!(
            run_value("return (-0).toExponential(2);"),
            string("0.00e+0")
        );
        assert_eq!(run_value("return (-0.0001).toFixed(2);"), string("-0.00"));
        assert_eq!(run_value("return (0.5).toFixed(0);"), string("1"));
        assert_eq!(run_value("return (2.5).toFixed(0);"), string("3"));
        assert_eq!(run_value("return (-2.5).toFixed(0);"), string("-3"));
        assert_eq!(run_value("return (0.25).toFixed(1);"), string("0.3"));
        assert_eq!(run_value("return (0.25).toPrecision(1);"), string("0.3"));
        assert_eq!(run_value("return (2.5).toPrecision(1);"), string("3"));
        assert_eq!(run_value("return (1.005).toPrecision(2);"), string("1.0"));
        assert_eq!(
            run_value("return (0.000001).toPrecision(2);"),
            string("0.0000010")
        );
        assert_eq!(run_value("return (1e-7).toPrecision(2);"), string("1.0e-7"));
        assert_eq!(
            run_value("return (2.25).toExponential(1);"),
            string("2.3e+0")
        );
        assert_eq!(
            run_value("return (9.99).toExponential(1);"),
            string("1.0e+1")
        );
        assert_eq!(
            run_value("return (999.5).toExponential(0);"),
            string("1e+3")
        );
        assert_eq!(
            run_value("return (0.25).toExponential(1);"),
            string("2.5e-1")
        );
        assert_eq!(run_value("return 'a'.repeat(-0.5);"), string(""));
        assert_eq!(run_value("return 'a'.repeat(2.9);"), string("aa"));
        let error = run("return 'a'.repeat(-1);").expect_err("negative repeat");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
        let error = run("return 'a'.repeat(Infinity);").expect_err("infinite repeat");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
        assert_eq!(
            run_value("return (9.999999999999999e20).toExponential(2);"),
            string("1.00e+21")
        );

        assert_eq!(
            run_value(
                r#"return [Number.isNaN("NaN"), Number.isFinite("5"), Number.isInteger("5"), Number.isSafeInteger(5), Number.isSafeInteger(9007199254740992)].join(",");"#
            ),
            string("false,false,false,true,false")
        );
        assert_eq!(run_value("return Number.isNaN(NaN);"), Value::Boolean(true));
        assert_eq!(
            run_value("return Number.isFinite(5);"),
            Value::Boolean(true)
        );

        assert_eq!(run_value("return String(1e21);"), string("1e+21"));
        assert_eq!(
            run_value("return String(1e20);"),
            string("100000000000000000000")
        );
        assert_eq!(run_value("return String(1e-7);"), string("1e-7"));
        assert_eq!(run_value("return String(1e-6);"), string("0.000001"));
        assert_eq!(run_value("return String(-0);"), string("0"));
        assert_eq!(
            run_value("return String(0.1 + 0.2);"),
            string("0.30000000000000004")
        );

        let error = run("return (5).toString(1);").expect_err("invalid radix");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
        let error = run("return (1).toPrecision(0);").expect_err("invalid precision");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
    }

    #[test]
    fn matches_object_static_restrictions() {
        assert_eq!(
            run_value(r#"return Object.keys([10, 20]).join(",");"#),
            string("0,1")
        );
        assert_eq!(
            run_value("return Object.values(new Map([['a', 1]])).length;"),
            number(0.0)
        );
        assert_eq!(
            run_value("return Object.entries(new Date(0)).length;"),
            number(0.0)
        );
        assert_eq!(
            run_value(
                "const a = { x: 1 }; const b = Object.assign(a, { y: 2 }); return [a.y === undefined, b.x, b.y].join(',');"
            ),
            string("true,1,2")
        );
        assert_eq!(
            run_value("return Object.keys(Object.assign({}, new Map([['a', 1]]))).length;"),
            number(0.0)
        );

        let error = run(r#"return Object.keys("ab");"#).expect_err("primitive keys");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        let error = run("return Object.values([1, 2]);").expect_err("array values");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        let error = run("return Object.entries([1, 2]);").expect_err("array entries");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        let error = run("return Object.assign({}, [1]);").expect_err("array source");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
    }

    #[test]
    fn matches_upstream_date_map_set_examples() {
        assert_eq!(run_value("return typeof Date.now();"), string("number"));
        assert_eq!(
            run_value("return new Date(0).toISOString();"),
            string("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value(r#"return new Date("2024-01-02T03:04:05.000Z").getTime();"#),
            number(1_704_164_645_000.0)
        );
        assert_eq!(
            run_value("const a = new Date(1000); const b = new Date(3000); return b - a;"),
            number(2000.0)
        );
        assert_eq!(run_value("return +new Date(42);"), number(42.0));
        assert_eq!(
            run_value(
                r#"const d = new Date("2024-03-05T06:07:08.009Z"); return [d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds(), d.getUTCMilliseconds()].join(",");"#
            ),
            string("2024,2,5,6,7,8,9")
        );
        assert_eq!(
            run_value(r#"return Number.isNaN(new Date("garbage").getTime());"#),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(r#"return new Date("garbage").toJSON();"#),
            Value::Null
        );
        assert_eq!(
            run_value(
                r#"try { new Date("garbage").toISOString(); return "no"; } catch { return "caught"; }"#
            ),
            string("caught")
        );
        assert_eq!(
            run_value("return `at ${new Date(0)}`;"),
            string("at 1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value("return JSON.stringify({ d: new Date(0) });"),
            string(r#"{"d":"1970-01-01T00:00:00.000Z"}"#)
        );
        assert_eq!(run_value("return Number(new Date(5));"), number(5.0));
        assert_eq!(
            run_value("return String(new Date(0));"),
            string("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            run_value("return Boolean(new Date(0));"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(
                "const dates = [new Date(3000), new Date(1000), new Date(2000)]; return dates.sort((a, b) => a - b).map((d) => d.getTime()).join(',');"
            ),
            string("1000,2000,3000")
        );
        assert_eq!(
            run_value(
                "const d = new Date(2024, 0, 2); return [d.getFullYear(), d.getMonth(), d.getDate()].join(',');"
            ),
            string("2024,0,2")
        );
        assert_eq!(run_value("return typeof new Date(0);"), string("object"));
        assert_eq!(
            run_value("return new Date(0).nope === undefined;"),
            Value::Boolean(true)
        );

        assert_eq!(
            run_value(
                "const m = new Map(); m.set('a', 1).set('b', 2); return [m.get('a'), m.get('b'), m.has('a'), m.get('zz') === undefined, m.size].join(',');"
            ),
            string("1,2,true,true,2")
        );
        assert_eq!(
            run_value(
                "const key = { id: 1 }; const m = new Map(); m.set(key, 'hit'); return [m.get(key), m.get({ id: 1 }) === undefined].join(',');"
            ),
            string("hit,true")
        );
        assert_eq!(
            run_value(
                "const m = new Map([['a', 1]]); const n = new Map(m); n.set('b', 2); return [n.get('a'), n.get('b'), m.has('b')].join(',');"
            ),
            string("1,2,false")
        );
        assert_eq!(
            run_value(
                "const m = new Map([['a', 1], ['b', 2]]); return JSON.stringify([m.keys(), m.values(), m.entries()]);"
            ),
            string(r#"[["a","b"],[1,2],[["a",1],["b",2]]]"#)
        );
        assert_eq!(
            run_value(
                "const m = new Map([['a', 1], ['b', 2]]); let total = 0; let names = ''; for (const [key, count] of m) { names += key; total += count; } return names + total;"
            ),
            string("ab3")
        );
        assert_eq!(
            run_value(
                "const m = new Map([['a', 1], ['b', 2]]); const seen = []; m.forEach((count, key) => seen.push(key + count)); return seen.join(',');"
            ),
            string("a1,b2")
        );
        assert_eq!(
            run_value(
                "const words = ['a', 'b', 'a', 'c', 'a']; const counts = new Map(); for (const word of words) counts.set(word, (counts.get(word) ?? 0) + 1); return JSON.stringify(Object.fromEntries(counts));"
            ),
            string(r#"{"a":3,"b":1,"c":1}"#)
        );

        assert_eq!(
            run_value(
                "const s = new Set(); s.add(1).add(2).add(1); const removed = s.delete(2); return [s.size, s.has(1), s.has(2), removed].join(',');"
            ),
            string("1,true,false,true")
        );
        assert_eq!(
            run_value("return [...new Set([1, 2, 2, 3, 1])].join(',');"),
            string("1,2,3")
        );
        assert_eq!(
            run_value("return [...new Set('aba')].join(',');"),
            string("a,b")
        );
        assert_eq!(
            run_value("return Array.from(new Set(new Set([1, 2]))).join(',');"),
            string("1,2")
        );
        assert_eq!(
            run_value("const s = new Set([NaN]); return s.has(NaN);"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(
                "let total = 0; for (const n of new Set([1, 2, 3])) total += n; return total;"
            ),
            number(6.0)
        );
    }

    #[test]
    fn matches_upstream_defensive_javascript_parity() {
        assert_eq!(
            run_value(r#"const s = "hi"; return s.login === undefined;"#),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(r#"const s = "hi"; return s?.login ?? "fallback";"#),
            string("fallback")
        );
        assert_eq!(run_value("return (5).foo ?? 'n';"), string("n"));
        assert_eq!(
            run_value("return [1,2,3].toSpliced === undefined;"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(r#"return typeof foo === "undefined" ? "safe" : "no";"#),
            string("safe")
        );
        assert_eq!(
            run_value("const o = null; return { ...o, a: 1 }.a;"),
            number(1.0)
        );
        assert_eq!(
            run_value("function f(opts) { return { ...opts, a: 1 } } return f(undefined).a;"),
            number(1.0)
        );
        assert!(run("return { ...[1,2], a: 1 };").is_err());
        assert_eq!(
            run_value("const o = {}; o.count = (o.count || 0) + 1; return o.count;"),
            number(1.0)
        );
        assert_eq!(
            run_value("return [3,1,2].reduce((a, b) => Math.max(a, b), -Infinity);"),
            number(3.0)
        );
        assert_eq!(
            run_value("return JSON.stringify({ x: Number('z') });"),
            string(r#"{"x":null}"#)
        );

        assert_eq!(
            run_value(
                r#"const e = new Error("boom"); return [e instanceof Error, e.name, e.message].join(",");"#
            ),
            string("true,Error,boom")
        );
        assert_eq!(
            run_value(
                r#"const e = Error("plain"); return [e instanceof Error, e.name, e.message].join(",");"#
            ),
            string("true,Error,plain")
        );
        assert_eq!(
            run_value(
                r#"const e = new TypeError("t"); return [e instanceof TypeError, e instanceof Error, e instanceof RangeError].join(",");"#
            ),
            string("true,true,false")
        );
        assert_eq!(
            run_value(
                r#"try { throw new Error("x"); } catch (e) { return e instanceof Error && e.message === "x"; }"#
            ),
            Value::Boolean(true)
        );
        assert_eq!(run_value("return typeof Error;"), string("function"));
        assert_eq!(
            run_value(
                r#"try { JSON.parse("{oops") } catch (e) { return [e.name, e instanceof SyntaxError, e instanceof Error, e instanceof TypeError, e.message.includes("JSON")].join(","); }"#
            ),
            string("SyntaxError,true,true,false,true")
        );
        assert_eq!(
            run_value(
                r#"try { undeclared() } catch (e) { return [e.name, e instanceof ReferenceError].join(","); }"#
            ),
            string("ReferenceError,true")
        );
        assert_eq!(
            run_value(
                r#"try { const c = 1; c = 2; } catch (e) { return [e.name, e instanceof TypeError].join(","); }"#
            ),
            string("TypeError,true")
        );
        assert_eq!(run_value("let x = 7; x **= 2; return x;"), number(49.0));
        assert_eq!(run_value("let x = 7; x &= 3; return x;"), number(3.0));
        assert_eq!(
            run_value("let x = -7; x >>>= 1; return x;"),
            number(2_147_483_644.0)
        );
        assert_eq!(
            run_value(r#"return [0, 1, "", 2, null, 3].filter(Boolean).join(",");"#),
            string("1,2,3")
        );
        assert_eq!(
            run_value("return [1, 2, 3].map(String).join(',');"),
            string("1,2,3")
        );
        let error = run("return [1, 2, 3].map(42);").expect_err("non-callable callback");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);

        assert_eq!(run_value("return parseFloat('.5');"), number(0.5));
        assert_eq!(run_value("return parseFloat('  1.5abc');"), number(1.5));
        assert_eq!(run_value("return parseFloat('0x10');"), number(0.0));
        assert_eq!(run_value("return parseInt('0x10');"), number(16.0));
        assert_eq!(run_value("return parseInt('10', 8);"), number(8.0));
        assert_eq!(
            run_value("return Number.isNaN(parseInt('10', 1));"),
            Value::Boolean(true)
        );
        assert_eq!(run_value("return parseInt('0x10', 8);"), number(0.0));
        assert_eq!(
            run_value("return 1 / parseInt('-0');"),
            number(f64::NEG_INFINITY)
        );
        assert_eq!(run_value("return parseInt('08', 10);"), number(8.0));
        assert_eq!(run_value("return parseInt('10', Infinity);"), number(10.0));
        assert_eq!(
            run_value("return Number.isNaN(parseInt('10', -1));"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return parseInt('10', 4294967296);"),
            number(10.0)
        );
        assert_eq!(run_value("return parseInt('9'.repeat(40));"), number(1e40));
        assert_eq!(
            run_value(r#"return String.fromCharCode(Infinity) === "\u0000";"#),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value(r#"return String.fromCharCode(-1) === "\uFFFF";"#),
            Value::Boolean(true)
        );
        let error = run("return String.fromCharCode('65');")
            .expect_err("fromCharCode requires number arguments");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        let error = run("return String.fromCodePoint('65');")
            .expect_err("fromCodePoint requires number arguments");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        assert_eq!(run_value(r#"return Number("\uFEFF1");"#), number(1.0));
        assert_eq!(run_value(r#"return parseFloat("\uFEFF1.5");"#), number(1.5));
        assert_eq!(run_value(r#"return parseInt("\uFEFF10");"#), number(10.0));

        assert_eq!(
            run_value("const a = [1]; a[1] = a; return a.join('-');"),
            string("1-")
        );
        assert_eq!(
            run_value("const a = [1]; a[1] = a; return String(a);"),
            string("1,")
        );
        assert_eq!(
            run_value("const b = []; b[0] = [b]; return b.join();"),
            string("")
        );
        for source in [
            "return [1, 2].join(undefined);",
            "return [1, 2].join(null);",
            "return [1, 2].join('-', 'x');",
        ] {
            let error = run(source).expect_err("join only accepts a string separator");
            assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError, "{source}");
        }
        for source in [
            "return [1].includes();",
            "return [1].includes(1, 0, 2);",
            "return [1].includes(1, '0');",
            "return [1].indexOf(1, '0');",
            "return [1].slice('0');",
            "return [1].splice('0', 1);",
            "return [1].fill(1, '0');",
            "return [1].copyWithin('0');",
            "return [1].at('0');",
            "return [1].with('0', 2);",
            "return [1].flat('1');",
            "return [1].lastIndexOf(1, '0');",
        ] {
            let error = run(source).expect_err("array indices must be numbers");
            assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError, "{source}");
        }
        assert_eq!(
            run_value("const a = [1, 2]; const r = a.splice(); return r.length + ':' + a.length;"),
            string("0:2")
        );
        assert_eq!(
            run_value(
                "const a = [1, 2]; let first; const same = []; \
                 a.map((v, i, snapshot) => { if (!first) first = snapshot; \
                 same.push(snapshot === first); return v; }); \
                 return [a[0] === first[0], same.join(',')].join(':');"
            ),
            string("true:true,true")
        );
        assert_eq!(
            run_value("const a = [1]; const seen = []; a.map((v, i, s) => seen.push(s === a)); return seen[0];"),
            Value::Boolean(false)
        );
        for source in [
            "return [2, 1].sort(Math.max);",
            "return [2, 1].toSorted(Math.max);",
        ] {
            let error = run(source).expect_err("sort comparators must be user functions");
            assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError, "{source}");
        }
        assert_eq!(
            run_value("return [NaN].includes(NaN);"),
            Value::Boolean(true)
        );
        assert_eq!(run_value("return [NaN].indexOf(NaN);"), number(-1.0));
        assert_eq!(run_value("return [0].includes(-0);"), Value::Boolean(true));
        assert_eq!(run_value("return [0].indexOf(-0);"), number(0.0));
        assert_eq!(run_value("return 'ab'.repeat(3);"), string("ababab"));
        assert_eq!(run_value("return 'x'.repeat(NaN);"), string(""));
        assert_eq!(run_value("return 'x'.padStart(5, 'ab');"), string("ababx"));
        assert_eq!(run_value("return 'x'.padEnd(0);"), string("x"));

        assert_eq!(
            run_value(r#"return JSON.stringify({ a: 1, b: [2, 3] }, null, 2);"#),
            string("{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}")
        );
        assert_eq!(
            run_value(r#"return JSON.stringify({ a: {}, b: [] }, null, 2);"#),
            string("{\n  \"a\": {},\n  \"b\": []\n}")
        );
        assert_eq!(
            run_value(r#"return JSON.stringify({ a: 1 }, null, "ab");"#),
            string("{\nab\"a\": 1\n}")
        );
        assert_eq!(
            run_value(r#"return JSON.stringify({ a: 1 }, null, 20);"#),
            string("{\n          \"a\": 1\n}")
        );
        assert_eq!(
            run_value("return JSON.stringify({ a: 1 }, null, 0);"),
            string(r#"{"a":1}"#)
        );
        let error = run(r#"return JSON.stringify({ a: 1 }, ["a"]);"#)
            .expect_err("replacer arrays are unsupported");
        assert_eq!(error.kind(), super::RuntimeErrorKind::UnsupportedSyntax);
        let error = run("const a = []; a[0] = a; return JSON.stringify(a);")
            .expect_err("circular JSON should fail");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        assert!(error.message().contains("circular"));

        for source in [
            "return 'x'.repeat(Infinity);",
            "return 'x'.repeat(-Infinity);",
            "return 'x'.repeat(-1);",
            "return 'x'.padStart(Infinity);",
        ] {
            let error = run(source).expect_err("range-checked string size");
            assert_eq!(
                error.kind(),
                super::RuntimeErrorKind::RangeError,
                "{source}"
            );
        }
        assert_eq!(
            run_value("let d = new Date(1000); d += 1; return d;"),
            string("1970-01-01T00:00:01.000Z1")
        );
        assert_eq!(
            run_value(
                "const e = new Error('m'); const o = { ...e }; return [o.name, o.message, o instanceof Error].join(',');"
            ),
            string("Error,m,false")
        );
        assert_eq!(
            run("return 1 instanceof 2;")
                .expect_err("non-callable right-hand side")
                .kind(),
            super::RuntimeErrorKind::TypeError
        );
    }

    #[test]
    fn matches_upstream_property_order_and_sort_semantics() {
        assert_eq!(
            run_value(r#"return Object.keys({ b: 1, 2: 2, 1: 3 }).join(",");"#),
            string("1,2,b")
        );
        assert_eq!(
            run_value(r#"return Object.keys({ "01": 1, 0: 2, "4294967295": 3 }).join(",");"#),
            string("0,01,4294967295")
        );
        assert_eq!(
            run_value(r#"return Object.keys({ b: 1, "4294967294": 2, 0: 3 }).join(",");"#),
            string("0,4294967294,b")
        );
        assert_eq!(
            run_value(r#"return Object.keys({ "1.5": 1, "-1": 2 }).join(",");"#),
            string("1.5,-1")
        );
        assert_eq!(
            run_value(r#"return JSON.stringify({ b: 1, 2: 2, 1: 3 });"#),
            string(r#"{"1":3,"2":2,"b":1}"#)
        );
        assert_eq!(
            run_value(r#"return JSON.stringify(JSON.parse('{"b":1,"a":2,"2":3,"1":4}'));"#),
            string(r#"{"1":4,"2":3,"b":1,"a":2}"#)
        );
        assert_eq!(
            run_value(
                r#"let keys = ""; for (const key in { b: 1, 2: 2, 1: 3 }) keys += key; return keys;"#
            ),
            string("12b")
        );
        assert_eq!(
            run_value(r#"return Object.keys({ ...{ b: 1, 2: 2, 1: 3 } }).join(",");"#),
            string("1,2,b")
        );
        assert_eq!(
            run_value(r#"return Object.keys(Object.assign({}, { b: 1, 2: 2, 1: 3 })).join(",");"#),
            string("1,2,b")
        );
        assert_eq!(
            run_value(
                r#"return Object.entries({ b: 1, 2: 2, 1: 3 }).map(([key]) => key).join(",");"#
            ),
            string("1,2,b")
        );

        assert_eq!(
            run_value(
                "return [\"😀\", \"\u{E000}\"].sort().map((value) => value.codePointAt(0)).join(',');"
            ),
            string("128512,57344")
        );
        assert_eq!(
            run_value("return \"😀\" < \"\u{FFFF}\";"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return \"😀\" < \"\u{E000}\";"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return [undefined, 'z'].sort().map((value) => String(value)).join(',');"),
            string("z,undefined")
        );

        assert_eq!(
            run_value(r#"return "\u{FEFF} x \u{FEFF}".trim();"#),
            string("x")
        );
        assert_eq!(run_value(r#"return "\u{FEFF}x".trimStart();"#), string("x"));
        assert_eq!(run_value(r#"return "x\u{FEFF}".trimEnd();"#), string("x"));
    }

    #[test]
    fn matches_upstream_relational_and_addition_coercion() {
        assert_eq!(run_value("return [10] < [2];"), Value::Boolean(true));
        assert_eq!(run_value("return [] < [1];"), Value::Boolean(true));
        assert_eq!(run_value("return [1] < 2;"), Value::Boolean(true));
        assert_eq!(run_value("return '10' < 2;"), Value::Boolean(false));
        assert_eq!(run_value("return 0 / 0 <= 1;"), Value::Boolean(false));
        assert_eq!(run_value("return 0 / 0 >= 1;"), Value::Boolean(false));
        assert_eq!(run_value("return 0 / 0 < 1;"), Value::Boolean(false));
        assert_eq!(run_value("return 0 / 0 > 1;"), Value::Boolean(false));

        assert_eq!(run_value("return [1, 2] + 1;"), string("1,21"));
        assert_eq!(run_value("return 1 + [2, 3];"), string("12,3"));
        assert_eq!(run_value("return ({}) + 1;"), string("[object Object]1"));
        assert_eq!(run_value("return [] + 1;"), string("1"));
        assert_eq!(run_value("return [1] + 1;"), string("11"));
        assert_eq!(run_value("return [1] + [2];"), string("12"));
        assert_eq!(run_value("return new Map() + 1;"), string("[object Map]1"));
        assert_eq!(
            run_value("return new Date(0) + 1;"),
            string("1970-01-01T00:00:00.000Z1")
        );
        assert_eq!(run_value("return null + 1;"), number(1.0));
        assert_eq!(run_value("return true + 1;"), number(2.0));
        assert_eq!(
            run_value("return Number.isNaN(undefined + 1);"),
            Value::Boolean(true)
        );
        assert_eq!(run_value("return 1 + 2;"), number(3.0));

        assert_eq!(
            run_value(
                "return [undefined, 2, 1].sort((a, b) => a - b).map((value) => String(value)).join(',');"
            ),
            string("1,2,undefined")
        );
        assert_eq!(
            run_value("return [3, 1, 2].sort(() => NaN).join(',');"),
            string("3,1,2")
        );
        assert_eq!(
            run_value("return 1 / Math.round(-0.4);"),
            number(f64::NEG_INFINITY)
        );
        assert_eq!(run_value("return Math.round(-1.5);"), number(-1.0));

        assert_eq!(
            run_value("return new Date(1000) > 0;"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return new Date(1000) < 2000;"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return new Date(NaN) >= 0;"),
            Value::Boolean(false)
        );
        assert_eq!(
            run_value("return new Date(0) > new Date(-1);"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return new Date(0) === 0;"),
            Value::Boolean(false)
        );
        assert_eq!(run_value("return new Date(0) == 0;"), Value::Boolean(true));
        assert_eq!(
            run_value("return new Date(0) == '1970-01-01T00:00:00.000Z';"),
            Value::Boolean(false)
        );
        assert_eq!(run_value("return [] == false;"), Value::Boolean(true));
        assert_eq!(run_value("return [1] == 1;"), Value::Boolean(true));
        assert_eq!(run_value("return ['1'] == 1;"), Value::Boolean(true));
        assert_eq!(
            run_value("return ({}) == '[object Object]';"),
            Value::Boolean(true)
        );
        assert_eq!(run_value("return [] == [];"), Value::Boolean(false));
        assert_eq!(run_value("return null == false;"), Value::Boolean(false));
        assert_eq!(run_value("return null == 0;"), Value::Boolean(false));
        assert_eq!(run_value("return null == undefined;"), Value::Boolean(true));
        assert_eq!(run_value("return 0 == '';"), Value::Boolean(true));
        assert_eq!(run_value("return NaN == NaN;"), Value::Boolean(false));
        assert_eq!(
            run_value("return new Map() == '[object Map]';"),
            Value::Boolean(true)
        );
        assert_eq!(
            run_value("return String(new Error('m'));"),
            string("[object Object]")
        );
        assert_eq!(
            run_value("return new Error('m') + 1;"),
            string("[object Object]1")
        );
        assert_eq!(run_value("return 'length' in [];"), Value::Boolean(true));
        assert_eq!(run_value("return '0' in ['a'];"), Value::Boolean(true));
        assert_eq!(run_value("return 'a' in { a: 1 };"), Value::Boolean(true));
        assert_eq!(run_value("return 'b' in { a: 1 };"), Value::Boolean(false));
        let error = run("return 'length' in 'abc';").expect_err("string right-hand side");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);
        let error = run("return 'x' in null;").expect_err("null right-hand side");
        assert_eq!(error.kind(), super::RuntimeErrorKind::TypeError);

        assert_eq!(
            run_value("return [1, 2, 3].reduceRight((acc, value, index) => acc + index, 0);"),
            number(3.0)
        );
        assert_eq!(
            run_value("return [1, 2, 3].reduceRight((acc, value) => acc + value);"),
            number(6.0)
        );
        assert_eq!(
            run_value("return ['a', 'b', 'c'].reduceRight((acc, value) => acc + value);"),
            string("cba")
        );
        assert_eq!(
            run_value("return [1, 2, 3].reduce((acc, value, index) => acc + index, 0);"),
            number(3.0)
        );
    }

    #[test]
    fn enforces_the_wall_clock_timeout() {
        let allocator = Allocator::default();
        let program = parse_program(&allocator, "while (true) {}").expect("fixture should parse");
        let mut interpreter = Interpreter::new();
        interpreter.set_limits(&ExecutionLimits {
            timeout_ms: Some(1),
            ..ExecutionLimits::default()
        });
        let error = interpreter.run(&program).expect_err("should time out");
        assert_eq!(error.kind(), super::RuntimeErrorKind::Timeout);
    }

    #[test]
    fn enforces_the_maximum_evaluation_depth() {
        let source = format!(
            "return {};",
            (0..200).map(|_| "1").collect::<Vec<_>>().join(" + ")
        );
        let error = run(&source).expect_err("should exceed the evaluation depth");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
    }

    #[test]
    fn enforces_the_maximum_call_depth() {
        let error = run("function recurse() { return recurse(); } return recurse();")
            .expect_err("should exceed the call depth");
        assert_eq!(error.kind(), super::RuntimeErrorKind::RangeError);
    }

    #[test]
    fn resolves_promise_combinators_synchronously() {
        let source = r#"
            const values = await Promise.all([tools.echo(1), tools.echo(2)]);
            const settled = await Promise.allSettled([tools.echo(3)]);
            return [values.join(","), settled[0].status, settled[0].value, await Promise.race([7, 8])];
        "#;
        let value = run_with_host(source).expect("program should run");
        let array = value.as_array().expect("array result");
        assert_eq!(array[0], string("1,2"));
        assert_eq!(array[1], string("fulfilled"));
        assert_eq!(array[2], number(3.0));
        assert_eq!(array[3], number(7.0));
    }
}
