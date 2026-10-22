//! Data-only values crossing the CodeMode boundary.
//!
//! The upstream interpreter keeps objects and arrays as null-prototype data
//! containers and rejects three member names at every boundary: `__proto__`,
//! `constructor`, and `prototype`. It also caps nesting at a fixed depth so a
//! pathological value produces a clear diagnostic instead of a host stack
//! overflow.
//!
//! Arrays and objects use shared references (`Rc<RefCell<...>>`) so that
//! JavaScript assignment aliases storage and identity comparison works, while
//! the data boundary still validates and copies the reachable data. Runtime-only
//! values (functions) may be stored inside arrays and objects but are rejected
//! by [`check_data`].

use std::cell::{Ref, RefCell};
use std::fmt::{self, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;

/// Maximum nesting depth for values crossing a data boundary, matching the
/// upstream `MAX_VALUE_DEPTH`.
pub const MAX_VALUE_DEPTH: usize = 32;

/// Maximum depth used when rendering a value as JSON.
///
/// Rendering never fails, so deeply nested or circular values are collapsed to
/// `null` instead of overflowing the stack.
const MAX_JSON_DEPTH: usize = 64;

/// Member names that must never be readable or writable in CodeMode.
pub const BLOCKED_MEMBERS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// Returns `true` when `name` is a blocked prototype-related member.
pub fn is_blocked_member(name: &str) -> bool {
    BLOCKED_MEMBERS.contains(&name)
}

/// Opaque handle to an interpreter-defined function.
///
/// Functions are runtime-only values: they may appear inside sandbox arrays and
/// objects, but they are rejected at the data boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(pub(crate) usize);

/// Shared storage for a JavaScript object.
pub type ObjectRef = Rc<RefCell<IndexMap<String, Value>>>;

/// Shared storage for a JavaScript array.
pub type ArrayRef = Rc<RefCell<Vec<Value>>>;

/// Brand carried by an `Error` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Generic `Error`.
    Error,
    /// `TypeError`.
    TypeError,
    /// `RangeError`.
    RangeError,
    /// `SyntaxError`.
    SyntaxError,
    /// `ReferenceError`.
    ReferenceError,
}

impl ErrorKind {
    /// Constructor name exposed as `error.name`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::SyntaxError => "SyntaxError",
            Self::ReferenceError => "ReferenceError",
        }
    }

    /// Resolves a constructor name to its brand.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Error" => Some(Self::Error),
            "TypeError" => Some(Self::TypeError),
            "RangeError" => Some(Self::RangeError),
            "SyntaxError" => Some(Self::SyntaxError),
            "ReferenceError" => Some(Self::ReferenceError),
            _ => None,
        }
    }
}

/// Shared error data.
#[derive(Debug)]
pub struct ErrorValue {
    /// Error brand used by `instanceof`.
    pub kind: ErrorKind,
    /// Human-readable message.
    pub message: String,
}

/// Shared reference to an error value.
pub type ErrorRef = Rc<ErrorValue>;

/// Settled state of a promise in the synchronous model.
#[derive(Debug)]
pub enum PromiseState {
    /// Promise completed with a value.
    Fulfilled(Value),
    /// Promise failed with a reason.
    Rejected(Value),
}

/// Shared reference to a settled promise.
pub type PromiseRef = Rc<PromiseState>;

/// Key wrapper providing SameValueZero equality and hashing for `Map` and
/// `Set` keys.
#[derive(Debug, Clone)]
struct CollectionKey(Value);

impl PartialEq for CollectionKey {
    fn eq(&self, other: &Self) -> bool {
        same_value_zero(&self.0, &other.0)
    }
}

impl Eq for CollectionKey {}

impl Hash for CollectionKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_value(&self.0, state);
    }
}

pub(crate) fn same_value_zero(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            left == right || (left.is_nan() && right.is_nan())
        }
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        _ => left.same_reference(right),
    }
}

fn hash_value<H: Hasher>(value: &Value, state: &mut H) {
    match value {
        Value::Undefined => 0u8.hash(state),
        Value::Null => 1u8.hash(state),
        Value::Boolean(value) => {
            2u8.hash(state);
            value.hash(state);
        }
        Value::Number(value) => {
            3u8.hash(state);
            let canonical = if value.is_nan() {
                f64::NAN.to_bits()
            } else if *value == 0.0 {
                0.0f64.to_bits()
            } else {
                value.to_bits()
            };
            canonical.hash(state);
        }
        Value::String(value) => {
            4u8.hash(state);
            value.hash(state);
        }
        Value::Array(value) => {
            5u8.hash(state);
            (Rc::as_ptr(value) as *const () as usize).hash(state);
        }
        Value::Object(value) => {
            6u8.hash(state);
            (Rc::as_ptr(value) as *const () as usize).hash(state);
        }
        Value::Function(value) => {
            7u8.hash(state);
            value.hash(state);
        }
        Value::Builtin(value) => {
            8u8.hash(state);
            value.hash(state);
        }
        Value::ToolRef(value) => {
            9u8.hash(state);
            value.as_slice().hash(state);
        }
        Value::Error(value) => {
            10u8.hash(state);
            (Rc::as_ptr(value) as *const () as usize).hash(state);
        }
        Value::Promise(value) => {
            11u8.hash(state);
            (Rc::as_ptr(value) as *const () as usize).hash(state);
        }
        Value::Map(value) => {
            12u8.hash(state);
            (Rc::as_ptr(value) as *const () as usize).hash(state);
        }
        Value::Set(value) => {
            13u8.hash(state);
            (Rc::as_ptr(value) as *const () as usize).hash(state);
        }
        Value::Date(value) => {
            15u8.hash(state);
            (Rc::as_ptr(value) as *const () as usize).hash(state);
        }
        Value::BoundMethod { receiver, method } => {
            14u8.hash(state);
            method.hash(state);
            hash_value(receiver, state);
        }
    }
}

/// Shared storage for a JavaScript `Map`.
#[derive(Debug, Default)]
pub struct MapValue {
    entries: RefCell<IndexMap<CollectionKey, Value>>,
}

impl MapValue {
    /// Creates an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a map from insertion-ordered entries.
    pub fn from_entries(entries: impl IntoIterator<Item = (Value, Value)>) -> Self {
        let map = entries
            .into_iter()
            .map(|(key, value)| (CollectionKey(key), value))
            .collect();
        Self {
            entries: RefCell::new(map),
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    /// Returns `true` when the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// Looks up a key using SameValueZero equality.
    pub fn get(&self, key: &Value) -> Option<Value> {
        self.entries
            .borrow()
            .get(&CollectionKey(key.clone()))
            .cloned()
    }

    /// Inserts an entry and returns the previous value.
    pub fn insert(&self, key: Value, value: Value) -> Option<Value> {
        self.entries.borrow_mut().insert(CollectionKey(key), value)
    }

    /// Removes an entry and returns its value.
    pub fn remove(&self, key: &Value) -> Option<Value> {
        self.entries
            .borrow_mut()
            .shift_remove(&CollectionKey(key.clone()))
    }

    /// Removes every entry.
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    /// Entries in insertion order.
    pub fn entries(&self) -> Vec<(Value, Value)> {
        self.entries
            .borrow()
            .iter()
            .map(|(key, value)| (key.0.clone(), value.clone()))
            .collect()
    }

    /// Keys in insertion order.
    pub fn keys(&self) -> Vec<Value> {
        self.entries
            .borrow()
            .keys()
            .map(|key| key.0.clone())
            .collect()
    }

    /// Values in insertion order.
    pub fn values(&self) -> Vec<Value> {
        self.entries.borrow().values().cloned().collect()
    }
}

/// Shared storage for a JavaScript `Set`.
#[derive(Debug, Default)]
pub struct SetValue {
    values: RefCell<IndexSet<CollectionKey>>,
}

impl SetValue {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a set from insertion-ordered values.
    pub fn from_values(values: impl IntoIterator<Item = Value>) -> Self {
        Self {
            values: RefCell::new(values.into_iter().map(CollectionKey).collect()),
        }
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        self.values.borrow().len()
    }

    /// Returns `true` when the set has no values.
    pub fn is_empty(&self) -> bool {
        self.values.borrow().is_empty()
    }

    /// Returns `true` when the value is present.
    pub fn has(&self, value: &Value) -> bool {
        self.values.borrow().contains(&CollectionKey(value.clone()))
    }

    /// Inserts a value and reports whether it was new.
    pub fn add(&self, value: Value) -> bool {
        self.values.borrow_mut().insert(CollectionKey(value))
    }

    /// Removes a value and reports whether it was present.
    pub fn remove(&self, value: &Value) -> bool {
        self.values
            .borrow_mut()
            .shift_remove(&CollectionKey(value.clone()))
    }

    /// Removes every value.
    pub fn clear(&self) {
        self.values.borrow_mut().clear();
    }

    /// Values in insertion order.
    pub fn values(&self) -> Vec<Value> {
        self.values
            .borrow()
            .iter()
            .map(|key| key.0.clone())
            .collect()
    }
}

/// Shared reference to a map.
pub type MapRef = Rc<MapValue>;

/// Shared reference to a set.
pub type SetRef = Rc<SetValue>;

/// Shared date data.
#[derive(Debug)]
pub struct DateValue {
    /// Unix time in milliseconds; `NaN` marks an invalid date.
    pub time_ms: f64,
}

/// Shared reference to a date.
pub type DateRef = Rc<DateValue>;

/// A value inside the confined interpreter.
///
/// Objects preserve insertion order, mirroring JavaScript own-key order for
/// string keys, and have no prototype chain.
#[derive(Debug, Clone)]
pub enum Value {
    /// JavaScript `undefined`.
    Undefined,
    /// JavaScript `null`.
    Null,
    /// JavaScript boolean.
    Boolean(bool),
    /// JavaScript number. Non-finite values are allowed in the sandbox and
    /// normalized to `null` only when leaving it.
    Number(f64),
    /// JavaScript string.
    String(String),
    /// JavaScript array. Holes are represented as [`Value::Undefined`].
    Array(ArrayRef),
    /// JavaScript null-prototype object.
    Object(ObjectRef),
    /// Interpreter-defined function.
    Function(FunctionId),
    /// Native global or namespace function, identified by its full path.
    Builtin(&'static str),
    /// Reference into the host tool tree, such as `tools.math.add`.
    ToolRef(Rc<Vec<String>>),
    /// JavaScript error value carrying a brand for `instanceof`.
    Error(ErrorRef),
    /// Settled promise value in the synchronous model.
    Promise(PromiseRef),
    /// JavaScript `Map`.
    Map(MapRef),
    /// JavaScript `Set`.
    Set(SetRef),
    /// JavaScript `Date`.
    Date(DateRef),
    /// Native method bound to a receiver, such as `array.map`.
    BoundMethod {
        /// Receiver value the method operates on.
        receiver: Box<Value>,
        /// Full method path, such as `Array.map`.
        method: &'static str,
    },
}

/// Returns the canonical array-index value of a property key, mirroring the
/// ECMAScript array-index definition (0 .. 2^32 - 2, no leading zeros).
pub(crate) fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let value = key.parse::<u32>().ok()?;
    (value != u32::MAX).then_some(value)
}

/// Orders object keys like ECMAScript: array indices ascending, then the
/// remaining keys in insertion order.
pub(crate) fn ordered_keys(object: &IndexMap<String, Value>) -> Vec<String> {
    let mut indices = Vec::new();
    let mut names = Vec::new();
    for key in object.keys() {
        match array_index(key) {
            Some(index) => indices.push((index, key.clone())),
            None => names.push(key.clone()),
        }
    }
    indices.sort_by_key(|(index, _)| *index);
    indices
        .into_iter()
        .map(|(_, key)| key)
        .chain(names)
        .collect()
}

impl Value {
    /// Creates an empty object.
    pub fn object() -> Self {
        Self::Object(Rc::new(RefCell::new(IndexMap::new())))
    }

    /// Creates an object from insertion-ordered entries.
    pub fn from_entries<I, K>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        let map = entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect();
        Self::Object(Rc::new(RefCell::new(map)))
    }

    /// Creates an array from values.
    pub fn array(values: Vec<Value>) -> Self {
        Self::Array(Rc::new(RefCell::new(values)))
    }

    /// Creates an error value with a brand.
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::Error(Rc::new(ErrorValue {
            kind,
            message: message.into(),
        }))
    }

    /// Creates a fulfilled promise.
    pub fn promise_fulfilled(value: Value) -> Self {
        Self::Promise(Rc::new(PromiseState::Fulfilled(value)))
    }

    /// Creates a rejected promise.
    pub fn promise_rejected(reason: Value) -> Self {
        Self::Promise(Rc::new(PromiseState::Rejected(reason)))
    }

    /// Creates an empty map.
    pub fn map() -> Self {
        Self::Map(Rc::new(MapValue::new()))
    }

    /// Creates an empty set.
    pub fn set() -> Self {
        Self::Set(Rc::new(SetValue::new()))
    }

    /// Creates a date, applying the JavaScript TimeClip algorithm.
    pub fn date(time_ms: f64) -> Self {
        Self::Date(Rc::new(DateValue {
            time_ms: crate::date::clip(time_ms),
        }))
    }

    /// Creates a reference into the host tool tree.
    pub fn tool_ref(path: Vec<String>) -> Self {
        Self::ToolRef(Rc::new(path))
    }

    /// Returns the tool path for a tool reference.
    pub fn tool_path(&self) -> Option<&[String]> {
        match self {
            Self::ToolRef(path) => Some(path.as_slice()),
            _ => None,
        }
    }

    /// Returns `true` for `undefined`.
    pub fn is_undefined(&self) -> bool {
        matches!(self, Self::Undefined)
    }

    /// Returns `true` for `null` or `undefined`.
    pub fn is_nullish(&self) -> bool {
        matches!(self, Self::Null | Self::Undefined)
    }

    /// Borrows the object map when this is an object.
    pub fn as_object(&self) -> Option<Ref<'_, IndexMap<String, Value>>> {
        match self {
            Self::Object(object) => Some(object.borrow()),
            _ => None,
        }
    }

    /// Borrows the array elements when this is an array.
    pub fn as_array(&self) -> Option<Ref<'_, Vec<Value>>> {
        match self {
            Self::Array(array) => Some(array.borrow()),
            _ => None,
        }
    }

    /// Returns `true` when both values refer to the same array, object, or
    /// function, matching JavaScript identity comparison.
    pub fn same_reference(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Array(left), Self::Array(right)) => Rc::ptr_eq(left, right),
            (Self::Object(left), Self::Object(right)) => Rc::ptr_eq(left, right),
            (Self::Function(left), Self::Function(right)) => left == right,
            (Self::Builtin(left), Self::Builtin(right)) => left == right,
            (
                Self::BoundMethod {
                    receiver: left_receiver,
                    method: left_method,
                },
                Self::BoundMethod {
                    receiver: right_receiver,
                    method: right_method,
                },
            ) => left_method == right_method && left_receiver.same_reference(right_receiver),
            (Self::ToolRef(left), Self::ToolRef(right)) => left == right,
            (Self::Error(left), Self::Error(right)) => Rc::ptr_eq(left, right),
            (Self::Promise(left), Self::Promise(right)) => Rc::ptr_eq(left, right),
            (Self::Map(left), Self::Map(right)) => Rc::ptr_eq(left, right),
            (Self::Set(left), Self::Set(right)) => Rc::ptr_eq(left, right),
            (Self::Date(left), Self::Date(right)) => Rc::ptr_eq(left, right),
            _ => false,
        }
    }

    /// Converts the value to its JSON boundary form.
    ///
    /// `undefined` and non-finite numbers become `null`, matching the upstream
    /// `copyOut` behavior and JSON stringification. Runtime-only values render
    /// as `null`; use [`check_data`] to reject them before serialization. Object
    /// key order follows `serde_json`'s map implementation; [`Value::Object`]
    /// itself preserves insertion order.
    pub fn to_json(&self) -> JsonValue {
        self.to_json_inner(&mut Vec::new())
    }

    fn to_json_inner(&self, ancestors: &mut Vec<usize>) -> JsonValue {
        match self {
            Self::Undefined | Self::Null => JsonValue::Null,
            Self::Boolean(value) => JsonValue::Bool(*value),
            Self::Number(value) if value.is_finite() => {
                serde_json::Number::from_f64(*value).map_or(JsonValue::Null, JsonValue::Number)
            }
            Self::Number(_) => JsonValue::Null,
            Self::String(value) => JsonValue::String(value.clone()),
            Self::Array(array) => {
                let identity = Rc::as_ptr(array) as *const () as usize;
                if ancestors.contains(&identity) || ancestors.len() >= MAX_JSON_DEPTH {
                    return JsonValue::Null;
                }
                ancestors.push(identity);
                let values = array
                    .borrow()
                    .iter()
                    .map(|value| value.to_json_inner(ancestors))
                    .collect();
                ancestors.pop();
                JsonValue::Array(values)
            }
            Self::Object(object) => {
                let identity = Rc::as_ptr(object) as *const () as usize;
                if ancestors.contains(&identity) || ancestors.len() >= MAX_JSON_DEPTH {
                    return JsonValue::Null;
                }
                ancestors.push(identity);
                let object = object.borrow();
                let mut map = JsonMap::with_capacity(object.len());
                for key in ordered_keys(&object) {
                    let value = object.get(&key).expect("ordered keys exist");
                    map.insert(key, value.to_json_inner(ancestors));
                }
                ancestors.pop();
                JsonValue::Object(map)
            }
            Self::Error(error) => {
                let mut map = JsonMap::new();
                map.insert(
                    "name".to_owned(),
                    JsonValue::String(error.kind.name().to_owned()),
                );
                map.insert(
                    "message".to_owned(),
                    JsonValue::String(error.message.clone()),
                );
                JsonValue::Object(map)
            }
            Self::Promise(_) => JsonValue::Null,
            Self::Map(_) | Self::Set(_) => JsonValue::Object(JsonMap::new()),
            Self::Date(date) => {
                crate::date::iso_string(date.time_ms).map_or(JsonValue::Null, JsonValue::String)
            }
            Self::Function(_) | Self::Builtin(_) | Self::BoundMethod { .. } | Self::ToolRef(_) => {
                JsonValue::Null
            }
        }
    }
}

impl PartialEq for Value {
    /// Structural for primitives, referential for arrays, objects, and
    /// functions. `NaN` never equals itself, matching JavaScript `===`.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Undefined, Self::Undefined) | (Self::Null, Self::Null) => true,
            (Self::Boolean(left), Self::Boolean(right)) => left == right,
            (Self::Number(left), Self::Number(right)) => left == right,
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Array(_), Self::Array(_))
            | (Self::Object(_), Self::Object(_))
            | (Self::Function(_), Self::Function(_))
            | (Self::Builtin(_), Self::Builtin(_))
            | (Self::BoundMethod { .. }, Self::BoundMethod { .. })
            | (Self::ToolRef(_), Self::ToolRef(_))
            | (Self::Error(_), Self::Error(_))
            | (Self::Promise(_), Self::Promise(_))
            | (Self::Map(_), Self::Map(_))
            | (Self::Set(_), Self::Set(_))
            | (Self::Date(_), Self::Date(_)) => self.same_reference(other),
            _ => false,
        }
    }
}

/// A value crossing the data boundary violated the plain-data contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataError {
    /// The value nested deeper than [`MAX_VALUE_DEPTH`].
    #[error("{label} exceeds the maximum value depth of {max}.")]
    DepthExceeded {
        /// Diagnostic label for the value.
        label: String,
        /// Maximum allowed depth.
        max: usize,
    },
    /// The object contained a blocked member name.
    #[error("{label} contains blocked property '{property}'.")]
    BlockedProperty {
        /// Diagnostic label for the value.
        label: String,
        /// Blocked member name.
        property: String,
    },
    /// The value contained a runtime-only value such as a function.
    #[error("{label} must contain data only.")]
    NonData {
        /// Diagnostic label for the value.
        label: String,
    },
    /// The value contains a cycle that cannot cross the boundary.
    #[error("{label} contains a circular value.")]
    Circular {
        /// Diagnostic label for the value.
        label: String,
    },
}

impl Display for Value {
    /// Renders the value in a JSON-like debug form for diagnostics.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undefined => formatter.write_str("undefined"),
            Self::Null => formatter.write_str("null"),
            Self::Boolean(value) => write!(formatter, "{value}"),
            Self::Number(value) => write!(formatter, "{value}"),
            Self::String(value) => write!(formatter, "{value:?}"),
            Self::Array(_) | Self::Object(_) => write!(formatter, "{}", self.to_json()),
            Self::Function(_) | Self::Builtin(_) | Self::BoundMethod { .. } => {
                formatter.write_str("[Function]")
            }
            Self::Error(error) => {
                if error.message.is_empty() {
                    formatter.write_str(error.kind.name())
                } else {
                    write!(formatter, "{}: {}", error.kind.name(), error.message)
                }
            }
            Self::Promise(_) => formatter.write_str("[Promise]"),
            Self::Map(_) => formatter.write_str("[object Map]"),
            Self::Set(_) => formatter.write_str("[object Set]"),
            Self::Date(date) => match crate::date::iso_string(date.time_ms) {
                Some(text) => formatter.write_str(&text),
                None => formatter.write_str("Invalid Date"),
            },
            Self::ToolRef(path) => {
                if path.is_empty() {
                    formatter.write_str("[tools]")
                } else {
                    formatter.write_str("[Function]")
                }
            }
        }
    }
}

/// Validates a value against the data boundary: depth limit, circularity, and
/// blocked member names, recursively.
pub fn check_data(value: &Value, label: &str) -> Result<(), DataError> {
    let mut seen = Vec::new();
    check_data_at(value, label, 0, &mut seen)
}

fn check_data_at(
    value: &Value,
    label: &str,
    depth: usize,
    seen: &mut Vec<*const ()>,
) -> Result<(), DataError> {
    if depth > MAX_VALUE_DEPTH {
        return Err(DataError::DepthExceeded {
            label: label.to_owned(),
            max: MAX_VALUE_DEPTH,
        });
    }

    match value {
        Value::Undefined
        | Value::Null
        | Value::Boolean(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Error(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Date(_) => Ok(()),
        Value::Function(_)
        | Value::Builtin(_)
        | Value::BoundMethod { .. }
        | Value::ToolRef(_)
        | Value::Promise(_) => Err(DataError::NonData {
            label: label.to_owned(),
        }),
        Value::Array(array) => {
            let pointer = Rc::as_ptr(array).cast::<()>();
            if seen.contains(&pointer) {
                return Err(DataError::Circular {
                    label: label.to_owned(),
                });
            }
            seen.push(pointer);
            let result = array
                .borrow()
                .iter()
                .try_for_each(|item| check_data_at(item, label, depth + 1, seen));
            seen.pop();
            result
        }
        Value::Object(object) => {
            let pointer = Rc::as_ptr(object).cast::<()>();
            if seen.contains(&pointer) {
                return Err(DataError::Circular {
                    label: label.to_owned(),
                });
            }
            seen.push(pointer);
            let result = {
                let object = object.borrow();
                object.iter().try_for_each(|(key, item)| {
                    if is_blocked_member(key) {
                        return Err(DataError::BlockedProperty {
                            label: label.to_owned(),
                            property: key.clone(),
                        });
                    }
                    check_data_at(item, label, depth + 1, seen)
                })
            };
            seen.pop();
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::{check_data, is_blocked_member, DataError, Value, MAX_VALUE_DEPTH};

    fn object(entries: &[(&str, Value)]) -> Value {
        Value::from_entries(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone())),
        )
    }

    #[test]
    fn blocks_prototype_related_members() {
        for name in ["__proto__", "constructor", "prototype"] {
            assert!(is_blocked_member(name), "{name} should be blocked");
        }
        assert!(!is_blocked_member("toString"));
    }

    #[test]
    fn rejects_blocked_properties_at_any_depth() {
        let value = object(&[(
            "nested",
            Value::array(vec![object(&[("constructor", Value::Null)])]),
        )]);

        assert_eq!(
            check_data(&value, "tool result"),
            Err(DataError::BlockedProperty {
                label: "tool result".to_owned(),
                property: "constructor".to_owned(),
            })
        );
    }

    #[test]
    fn enforces_the_fixed_depth_limit() {
        fn nested(depth: usize) -> Value {
            let mut value = Value::Null;
            for _ in 0..depth {
                value = Value::array(vec![value]);
            }
            value
        }

        assert!(check_data(&nested(MAX_VALUE_DEPTH), "value").is_ok());
        assert_eq!(
            check_data(&nested(MAX_VALUE_DEPTH + 1), "value"),
            Err(DataError::DepthExceeded {
                label: "value".to_owned(),
                max: MAX_VALUE_DEPTH,
            })
        );
    }

    #[test]
    fn rejects_circular_values() {
        let array = Rc::new(RefCell::new(Vec::new()));
        let value = Value::Array(Rc::clone(&array));
        array.borrow_mut().push(value.clone());

        assert_eq!(
            check_data(&value, "value"),
            Err(DataError::Circular {
                label: "value".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_functions() {
        let value = Value::array(vec![Value::Function(super::FunctionId(0))]);

        assert_eq!(
            check_data(&value, "value"),
            Err(DataError::NonData {
                label: "value".to_owned(),
            })
        );
    }

    #[test]
    fn normalizes_non_finite_numbers_and_undefined_at_the_boundary() {
        let value = Value::array(vec![
            Value::Undefined,
            Value::Number(f64::NAN),
            Value::Number(f64::INFINITY),
            Value::Number(1.5),
            Value::String("text".to_owned()),
        ]);

        assert_eq!(
            value.to_json().to_string(),
            r#"[null,null,null,1.5,"text"]"#
        );
    }

    #[test]
    fn preserves_object_insertion_order_internally() {
        let value = object(&[
            ("z", Value::Number(1.0)),
            ("a", Value::Number(2.0)),
            ("m", Value::Number(3.0)),
        ]);

        let object = value.as_object().expect("object value");
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(keys, ["z", "a", "m"]);
        assert_eq!(value.to_json().to_string(), r#"{"z":1.0,"a":2.0,"m":3.0}"#);
    }

    #[test]
    fn orders_object_json_keys_like_javascript() {
        let value = object(&[
            ("b", Value::Number(1.0)),
            ("2", Value::Number(2.0)),
            ("1", Value::Number(3.0)),
        ]);

        assert_eq!(value.to_json().to_string(), r#"{"1":3.0,"2":2.0,"b":1.0}"#);
    }

    #[test]
    fn arrays_and_objects_compare_by_reference() {
        let left = Value::array(vec![Value::Number(1.0)]);
        let same = left.clone();
        let right = Value::array(vec![Value::Number(1.0)]);

        assert_eq!(left, same);
        assert_ne!(left, right);
    }
}
