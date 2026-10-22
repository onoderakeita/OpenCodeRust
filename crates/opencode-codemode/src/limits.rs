//! Execution resource limits matching the upstream CodeMode runtime.
//!
//! The three limits are optional and independent:
//!
//! - `timeoutMs` is wall-clock time and must be at least 1.
//! - `maxToolCalls` counts admitted tool calls and must be at least 0.
//! - `maxOutputBytes` budgets model-facing UTF-8 output and must be at least 0.
//!
//! Absent limits mean unlimited, matching upstream. Output truncation is
//! deliberately lossy but never an execution failure: an oversized value is
//! replaced by truncated serialized text plus a marker, and logs are kept from
//! the start until the remaining budget is exhausted.

use std::fmt::{self, Display, Formatter};

/// Maximum wall-clock time in milliseconds.
pub const MIN_TIMEOUT_MS: u64 = 1;

/// Per-execution overrides for the default resource limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecutionLimits {
    /// Wall-clock timeout in milliseconds. `None` means no timeout.
    pub timeout_ms: Option<u64>,
    /// Maximum admitted tool calls. `None` means unlimited.
    pub max_tool_calls: Option<usize>,
    /// Maximum model-facing UTF-8 output bytes. `None` means unbounded.
    pub max_output_bytes: Option<usize>,
}

impl ExecutionLimits {
    /// Validates and constructs a limit set.
    pub fn new(
        timeout_ms: Option<u64>,
        max_tool_calls: Option<usize>,
        max_output_bytes: Option<usize>,
    ) -> Result<Self, LimitError> {
        if let Some(timeout) = timeout_ms {
            if timeout < MIN_TIMEOUT_MS {
                return Err(LimitError::Invalid {
                    name: "timeoutMs",
                    minimum: MIN_TIMEOUT_MS as usize,
                });
            }
        }
        Ok(Self {
            timeout_ms,
            max_tool_calls,
            max_output_bytes,
        })
    }
}

/// A limit configuration rejected at construction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitError {
    /// The named limit was below its minimum.
    #[error("{name} must be a safe integer greater than or equal to {minimum}.")]
    Invalid {
        /// Limit name as exposed to callers.
        name: &'static str,
        /// Inclusive minimum.
        minimum: usize,
    },
    /// The tool-call budget was exhausted.
    #[error("Execution exceeded its tool-call limit of {limit}.")]
    ToolCallLimitExceeded {
        /// Configured maximum.
        limit: usize,
    },
}

/// Tracks admitted tool calls against an optional budget.
#[derive(Debug, Clone)]
pub struct ToolCallBudget {
    limit: Option<usize>,
    calls: Vec<String>,
}

impl ToolCallBudget {
    /// Creates a budget; `None` means unlimited.
    pub fn new(limit: Option<usize>) -> Self {
        Self {
            limit,
            calls: Vec::new(),
        }
    }

    /// Records an admitted call and returns its zero-based index.
    pub fn admit(&mut self, name: impl Into<String>) -> Result<usize, LimitError> {
        if let Some(limit) = self.limit {
            if self.calls.len() >= limit {
                return Err(LimitError::ToolCallLimitExceeded { limit });
            }
        }
        self.calls.push(name.into());
        Ok(self.calls.len() - 1)
    }

    /// Number of admitted calls.
    pub fn len(&self) -> usize {
        self.calls.len()
    }

    /// Returns `true` when no call has been admitted.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Admitted call names in invocation order.
    pub fn calls(&self) -> &[String] {
        &self.calls
    }
}

/// Truncates `value` to at most `max_bytes` UTF-8 bytes without splitting a
/// code point.
pub fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Bounds a serialized value to `max_bytes`.
///
/// Returns `None` when the value fits, or the truncated replacement with the
/// upstream marker when it does not.
pub fn bound_serialized_value(serialized: &str, max_bytes: usize) -> Option<String> {
    if serialized.len() <= max_bytes {
        return None;
    }
    Some(format!(
        "{} [result truncated: {} bytes exceeds the {}-byte output limit; return a smaller value]",
        truncate_utf8(serialized, max_bytes),
        serialized.len(),
        max_bytes
    ))
}

/// Result of applying an output budget to logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedLogs {
    /// Log lines that fit in the remaining budget.
    pub lines: Vec<String>,
    /// `true` when any line was dropped.
    pub truncated: bool,
}

/// Keeps log lines from the start until `budget` bytes are exhausted.
///
/// Each line is charged its UTF-8 byte length plus one for the line break,
/// matching the upstream accounting. A truncation marker line is appended when
/// lines are dropped; the marker itself is not charged against the budget.
pub fn bound_logs(lines: &[String], budget: usize) -> BoundedLogs {
    let mut kept = Vec::new();
    let mut used = 0;
    for line in lines {
        let cost = line.len() + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        kept.push(line.clone());
    }

    let truncated = kept.len() < lines.len();
    if truncated {
        kept.push(format!(
            "[logs truncated: showing {} of {} lines]",
            kept.len(),
            lines.len()
        ));
    }

    BoundedLogs {
        lines: kept,
        truncated,
    }
}

/// Model-facing output after applying [`ExecutionLimits::max_output_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    /// Possibly truncated value text.
    pub value: String,
    /// Kept log lines.
    pub logs: Vec<String>,
    /// `true` when the value or logs were truncated.
    pub truncated: bool,
}

/// Applies an output byte budget to a serialized value and its logs.
///
/// The value is charged first; logs use the remaining budget.
pub fn bound_output(serialized_value: &str, logs: &[String], max_bytes: usize) -> BoundedOutput {
    let value = match bound_serialized_value(serialized_value, max_bytes) {
        Some(truncated) => truncated,
        None => serialized_value.to_owned(),
    };
    let value_bytes = if value.len() > max_bytes {
        // The marker pushes the replacement over the budget; the budget charge is
        // capped at `max_bytes`, matching upstream.
        max_bytes
    } else {
        value.len()
    };

    let log_budget = max_bytes.saturating_sub(value_bytes);
    let bounded_logs = bound_logs(logs, log_budget);
    let value_truncated = value.len() > serialized_value.len();

    BoundedOutput {
        value,
        logs: bounded_logs.lines,
        truncated: value_truncated || bounded_logs.truncated,
    }
}

impl Display for BoundedOutput {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.value)?;
        for line in &self.logs {
            write!(formatter, "\n{line}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bound_logs, bound_output, bound_serialized_value, truncate_utf8, ExecutionLimits,
        LimitError, ToolCallBudget,
    };

    #[test]
    fn validates_timeout_minimum() {
        assert!(ExecutionLimits::new(Some(1), None, None).is_ok());
        assert_eq!(
            ExecutionLimits::new(Some(0), None, None),
            Err(LimitError::Invalid {
                name: "timeoutMs",
                minimum: 1,
            })
        );
    }

    #[test]
    fn enforces_tool_call_budget() {
        let mut budget = ToolCallBudget::new(Some(2));
        assert_eq!(budget.admit("search"), Ok(0));
        assert_eq!(budget.admit("fetch"), Ok(1));
        assert_eq!(
            budget.admit("fetch"),
            Err(LimitError::ToolCallLimitExceeded { limit: 2 })
        );
        assert_eq!(budget.calls(), ["search", "fetch"]);
    }

    #[test]
    fn truncates_on_utf8_boundaries() {
        let value = "aあb";
        assert_eq!(truncate_utf8(value, 4), "aあ");
        assert_eq!(truncate_utf8(value, 2), "a");
        assert_eq!(truncate_utf8(value, 10), value);
    }

    #[test]
    fn replaces_oversized_values_with_a_marker() {
        assert_eq!(bound_serialized_value(r#""small""#, 64), None);

        let truncated =
            bound_serialized_value("\"0123456789\"", 6).expect("value should be truncated");
        assert_eq!(
            truncated,
            "\"01234 [result truncated: 12 bytes exceeds the 6-byte output limit; return a smaller value]"
        );
    }

    #[test]
    fn keeps_logs_from_the_start_and_reports_the_remainder() {
        let logs = vec!["first".to_owned(), "second".to_owned(), "third".to_owned()];
        let bounded = bound_logs(&logs, 12);

        assert_eq!(
            bounded.lines,
            vec![
                "first".to_owned(),
                "[logs truncated: showing 1 of 3 lines]".to_owned()
            ]
        );
        assert!(bounded.truncated);
    }

    #[test]
    fn charges_the_value_before_logs() {
        let logs = vec!["log".to_owned()];
        let bounded = bound_output("\"0123456789\"", &logs, 6);

        assert_eq!(
            bounded.value,
            "\"01234 [result truncated: 12 bytes exceeds the 6-byte output limit; return a smaller value]"
        );
        assert_eq!(
            bounded.logs,
            vec!["[logs truncated: showing 0 of 1 lines]".to_owned()]
        );
        assert!(bounded.truncated);
    }

    #[test]
    fn leaves_output_untouched_when_it_fits() {
        let bounded = bound_output("\"ok\"", &["log".to_owned()], 64);

        assert_eq!(bounded.value, "\"ok\"");
        assert_eq!(bounded.logs, ["log"]);
        assert!(!bounded.truncated);
    }
}
