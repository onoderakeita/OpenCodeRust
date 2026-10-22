//! Data-only values crossing the CodeMode boundary.
//!
//! The upstream interpreter keeps objects and arrays as null-prototype data
//! containers and rejects three member names at every boundary: `__proto__`,
//! `constructor`, and `prototype`. It also caps nesting at a fixed depth so a
//! pathological value produces a clear diagnostic instead of a host stack
//! overflow.
//!
//! This module implements the data subset only. Runtime-only values (functions,
//! promises, tool references, sandbox `Date`/`RegExp`/`Map`/`Set` instances)
//! belong to the evaluator and are added there; they must never cross this
//! boundary as plain objects.

use std::fmt::{self, Display, Formatter};

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

/// A data value inside the confined interpreter.
///
/// Objects preserve insertion order, mirroring JavaScript own-key order for
/// string keys, and have no prototype chain.
#[derive(Debug, Clone, PartialEq)]
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
    Array(Vec<Value>),
    /// JavaScript null-prototype object.
    Object(IndexMap<String, Value>),
}

impl Value {
    /// Creates an empty object.
    pub fn object() -> Self {
        Self::Object(IndexMap::new())
    }

    /// Returns `true` for `undefined`.
    pub fn is_undefined(&self) -> bool {
        matches!(self, Self::Undefined)
    }

    /// Returns the object map when this is an object.
    pub fn as_object(&self) -> Option<&IndexMap<String, Value>> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    /// Returns the array slice when this is an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(array) => Some(array),
            _ => None,
        }
    }

    /// Converts the value to its JSON boundary form.
    ///
    /// `undefined` and non-finite numbers become `null`, matching the upstream
    /// `copyOut` behavior and JSON stringification. Object key order follows
    /// `serde_json`'s map implementation; [`Value::Object`] itself preserves
    /// insertion order.
    pub fn to_json(&self) -> JsonValue {
        match self {
            Self::Undefined | Self::Null => JsonValue::Null,
            Self::Boolean(value) => JsonValue::Bool(*value),
            Self::Number(value) if value.is_finite() => {
                serde_json::Number::from_f64(*value).map_or(JsonValue::Null, JsonValue::Number)
            }
            Self::Number(_) => JsonValue::Null,
            Self::String(value) => JsonValue::String(value.clone()),
            Self::Array(array) => JsonValue::Array(array.iter().map(Self::to_json).collect()),
            Self::Object(object) => {
                let mut map = JsonMap::with_capacity(object.len());
                for (key, value) in object {
                    map.insert(key.clone(), value.to_json());
                }
                JsonValue::Object(map)
            }
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
        }
    }
}

/// Validates a value against the data boundary: depth limit and blocked member
/// names, recursively.
pub fn check_data(value: &Value, label: &str) -> Result<(), DataError> {
    check_data_at(value, label, 0)
}

fn check_data_at(value: &Value, label: &str, depth: usize) -> Result<(), DataError> {
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
        Value::Array(array) => array
            .iter()
            .try_for_each(|item| check_data_at(item, label, depth + 1)),
        Value::Object(object) => {
            for (key, item) in object {
                if is_blocked_member(key) {
                    return Err(DataError::BlockedProperty {
                        label: label.to_owned(),
                        property: key.clone(),
                    });
                }
                check_data_at(item, label, depth + 1)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::{check_data, is_blocked_member, DataError, Value, MAX_VALUE_DEPTH};

    fn object(entries: &[(&str, Value)]) -> Value {
        Value::Object(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect::<IndexMap<_, _>>(),
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
            Value::Array(vec![object(&[("constructor", Value::Null)])]),
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
                value = Value::Array(vec![value]);
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
    fn normalizes_non_finite_numbers_and_undefined_at_the_boundary() {
        let value = Value::Array(vec![
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

        let keys: Vec<&str> = value
            .as_object()
            .expect("object value")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["z", "a", "m"]);
        assert_eq!(value.to_json().to_string(), r#"{"a":2.0,"m":3.0,"z":1.0}"#);
    }
}
