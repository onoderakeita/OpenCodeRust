//! Differential corpus for dependency-free standard-library behavior.
//!
//! Expected values were produced by Node.js and then reviewed for the places
//! where upstream CodeMode intentionally differs from plain JavaScript (UTC
//! dates, plain-object Error values). Only deterministic, dependency-free
//! expressions are included so the corpus runs without network or RegExp
//! support.

use opencode_codemode::{execute_code, CodeModeResult, ExecutionLimits};

/// Normalizes integral floats to integers so `16.0` and `16` compare equal.
fn normalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(number) => match number.as_f64() {
            Some(float) if float.is_finite() && float.fract() == 0.0 => {
                serde_json::Value::Number(serde_json::Number::from(float as i64))
            }
            _ => value.clone(),
        },
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalize).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), normalize(item)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[test]
fn dependency_free_stdlib_matches_the_reference_corpus() {
    let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("parity_cases.json"))
        .expect("parity corpus must be valid JSON");
    let limits = ExecutionLimits::default();
    let mut failures = Vec::new();

    for case in &cases {
        let expression = case["expr"].as_str().expect("expression");
        let expected = case.get("expected").expect("expected value");
        let source = format!("return {expression};");
        match execute_code(&source, None, &limits) {
            CodeModeResult::Success(success) => {
                if normalize(&success.value) != normalize(expected) {
                    failures.push(format!(
                        "{expression}\n  expected: {expected}\n  actual:   {}",
                        success.value
                    ));
                }
            }
            CodeModeResult::Failure(failure) => failures.push(format!(
                "{expression}\n  {}: {}",
                failure.error.kind, failure.error.message
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} corpus cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
