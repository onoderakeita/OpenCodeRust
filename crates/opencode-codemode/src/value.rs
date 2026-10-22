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
use std::rc::Rc;

use indexmap::IndexMap;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;

/// Maximum nesting depth for values crossing a data boundary, matching the
/// upstream `MAX_VALUE_DEPTH`.
pub const MAX_VALUE_DEPTH: usize = 32;

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
        match self {
            Self::Undefined | Self::Null => JsonValue::Null,
            Self::Boolean(value) => JsonValue::Bool(*value),
            Self::Number(value) if value.is_finite() => {
                serde_json::Number::from_f64(*value).map_or(JsonValue::Null, JsonValue::Number)
            }
            Self::Number(_) => JsonValue::Null,
            Self::String(value) => JsonValue::String(value.clone()),
            Self::Array(array) => {
                JsonValue::Array(array.borrow().iter().map(Self::to_json).collect())
            }
            Self::Object(object) => {
                let object = object.borrow();
                let mut map = JsonMap::with_capacity(object.len());
                for (key, value) in object.iter() {
                    map.insert(key.clone(), value.to_json());
                }
                JsonValue::Object(map)
            }
            Self::Function(_) => JsonValue::Null,
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
            | (Self::Function(_), Self::Function(_)) => self.same_reference(other),
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
            Self::Function(_) => formatter.write_str("[Function]"),
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
        | Value::String(_) => Ok(()),
        Value::Function(_) => Err(DataError::NonData {
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
        assert_eq!(value.to_json().to_string(), r#"{"a":2.0,"m":3.0,"z":1.0}"#);
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
