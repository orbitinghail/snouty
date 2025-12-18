use jsonschema::Validator;
use log::debug;
use serde_json::{Map, Value};

use crate::error::{Error, Result};

const SCHEMA: &str = include_str!("params_schema.json");

/// Params parsed from CLI arguments and validated against the JSON schema.
#[derive(Debug, Clone)]
pub struct Params {
    inner: Map<String, Value>,
}

impl Params {
    /// Parse params from CLI arguments.
    ///
    /// Arguments should be in the format: `--key value`
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let inner = parse_args(args)?;
        Ok(Self { inner })
    }

    /// Create params from a JSON value.
    ///
    /// The value must be a JSON object.
    pub fn from_json(value: &Value) -> Result<Self> {
        let inner = value
            .as_object()
            .ok_or_else(|| Error::InvalidArgs("expected JSON object".to_string()))?
            .clone();
        debug!("parsed {} params from JSON", inner.len());
        Ok(Self { inner })
    }

    /// Validate params against the test params schema.
    pub fn validate_test_params(&self) -> Result<()> {
        validate_against_def(&self.inner, "testParams")
    }

    /// Validate params against the debugging params schema.
    pub fn validate_debugging_params(&self) -> Result<()> {
        validate_against_def(&self.inner, "debuggingParams")
    }

    /// Get a reference to the inner map.
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.inner
    }

    /// Convert to a JSON value.
    pub fn to_value(&self) -> Value {
        Value::Object(self.inner.clone())
    }

    /// Merge another Params into this one, with the other params taking priority.
    pub fn merge(&mut self, other: Params) {
        for (key, value) in other.inner {
            self.inner.insert(key, value);
        }
    }

    /// Get a redacted copy of the params for safe display in logs/CI.
    /// Sensitive fields (tokens, emails) are replaced with "[REDACTED]".
    pub fn to_redacted_map(&self) -> Map<String, Value> {
        self.inner
            .iter()
            .map(|(k, v)| {
                let redacted = is_sensitive_key(k);
                let value = if redacted {
                    Value::String("[REDACTED]".to_string())
                } else {
                    v.clone()
                };
                (k.clone(), value)
            })
            .collect()
    }
}

fn is_sensitive_key(key: &str) -> bool {
    key.ends_with(".token") || key == "antithesis.report.recipients"
}

fn parse_args<I, S>(args: I) -> Result<Map<String, Value>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut map = Map::new();
    let mut iter = args.into_iter().peekable();

    while let Some(arg) = iter.next() {
        let arg = arg.as_ref();

        if let Some(key) = arg.strip_prefix("--") {
            if key.is_empty() {
                return Err(Error::InvalidArgs("empty key after --".to_string()));
            }

            let value = iter
                .next()
                .ok_or_else(|| Error::InvalidArgs(format!("missing value for --{}", key)))?;

            map.insert(key.to_string(), Value::String(value.as_ref().to_string()));
        } else {
            return Err(Error::InvalidArgs(format!("unexpected argument: {}", arg)));
        }
    }

    Ok(map)
}

fn validate_against_def(params: &Map<String, Value>, def_name: &str) -> Result<()> {
    let schema: Value = serde_json::from_str(SCHEMA).expect("valid schema");

    // Build a schema that references the specific definition
    let def_schema = serde_json::json!({
        "$ref": format!("#/$defs/{}", def_name),
        "$defs": schema["$defs"]
    });

    let validator = Validator::new(&def_schema).expect("valid schema");
    let instance = Value::Object(params.clone());

    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|e| e.to_string())
        .collect();

    if !errors.is_empty() {
        debug!("validation failed with {} errors", errors.len());
        for err in &errors {
            debug!("  - {}", err);
        }
        return Err(Error::ValidationFailed(errors));
    }

    debug!("validation passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_args() {
        let args = [
            "--antithesis.duration",
            "30",
            "--antithesis.description",
            "test run",
        ];
        let params = Params::from_args(args).unwrap();

        assert_eq!(params.as_map().get("antithesis.duration").unwrap(), "30");
        assert_eq!(
            params.as_map().get("antithesis.description").unwrap(),
            "test run"
        );
    }

    #[test]
    fn parse_values_as_strings() {
        let args = ["--count", "42", "--enabled", "true", "--ratio", "3.14"];
        let params = Params::from_args(args).unwrap();

        // Values are kept as strings (schema validates format)
        assert_eq!(params.as_map().get("count").unwrap(), "42");
        assert_eq!(params.as_map().get("enabled").unwrap(), "true");
        assert_eq!(params.as_map().get("ratio").unwrap(), "3.14");
    }

    #[test]
    fn parse_integration_args() {
        let args = [
            "--antithesis.integrations.github.callback_url",
            "https://github.com/cb",
            "--antithesis.integrations.github.token",
            "secret",
        ];
        let params = Params::from_args(args).unwrap();

        assert_eq!(
            params
                .as_map()
                .get("antithesis.integrations.github.callback_url")
                .unwrap(),
            "https://github.com/cb"
        );
        assert_eq!(
            params
                .as_map()
                .get("antithesis.integrations.github.token")
                .unwrap(),
            "secret"
        );
    }

    #[test]
    fn validate_test_params_success() {
        let args = [
            "--antithesis.duration",
            "30",
            "--antithesis.is_ephemeral",
            "true",
        ];
        let params = Params::from_args(args).unwrap();
        assert!(params.validate_test_params().is_ok());
    }

    #[test]
    fn validate_test_params_with_custom_props() {
        let args = [
            "--antithesis.duration",
            "30",
            "--my.custom.property",
            "value",
        ];
        let params = Params::from_args(args).unwrap();
        assert!(params.validate_test_params().is_ok());
    }

    #[test]
    fn validate_debugging_params_success() {
        let args = [
            "--antithesis.debugging.input_hash",
            "abc123",
            "--antithesis.debugging.session_id",
            "sess-456",
            "--antithesis.debugging.vtime",
            "1234567890",
        ];
        let params = Params::from_args(args).unwrap();
        assert!(params.validate_debugging_params().is_ok());
    }

    #[test]
    fn validate_debugging_params_missing_required() {
        let args = ["--antithesis.debugging.input_hash", "abc123"];
        let params = Params::from_args(args).unwrap();
        assert!(params.validate_debugging_params().is_err());
    }

    #[test]
    fn validate_debugging_params_rejects_custom_props() {
        let args = [
            "--antithesis.debugging.input_hash",
            "abc123",
            "--antithesis.debugging.session_id",
            "sess-456",
            "--antithesis.debugging.vtime",
            "123",
            "--my.custom.prop",
            "value",
        ];
        let params = Params::from_args(args).unwrap();
        assert!(params.validate_debugging_params().is_err());
    }

    #[test]
    fn missing_value_error() {
        let args = ["--antithesis.duration"];
        let result = Params::from_args(args);
        assert!(result.is_err());
    }

    #[test]
    fn unexpected_arg_error() {
        let args = ["notaflag", "value"];
        let result = Params::from_args(args);
        assert!(result.is_err());
    }

    #[test]
    fn empty_key_after_dashes_error() {
        let args = ["--", "value"];
        let result = Params::from_args(args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty key"));
    }

    #[test]
    fn merge_params_overwrites_existing_keys() {
        let mut base = Params::from_args([
            "--antithesis.duration",
            "30",
            "--antithesis.description",
            "base description",
        ])
        .unwrap();

        let overlay = Params::from_args([
            "--antithesis.duration",
            "60",
            "--antithesis.report.recipients",
            "team@example.com",
        ])
        .unwrap();

        base.merge(overlay);

        // Overlay value should overwrite base value
        assert_eq!(base.as_map().get("antithesis.duration").unwrap(), "60");
        // Base-only value should be preserved
        assert_eq!(
            base.as_map().get("antithesis.description").unwrap(),
            "base description"
        );
        // Overlay-only value should be added
        assert_eq!(
            base.as_map().get("antithesis.report.recipients").unwrap(),
            "team@example.com"
        );
    }

    #[test]
    fn redacted_map_hides_sensitive_values() {
        let args = [
            "--antithesis.duration",
            "30",
            "--antithesis.integrations.github.token",
            "secret_token_123",
            "--antithesis.integrations.github.callback_url",
            "https://example.com/callback",
            "--antithesis.report.recipients",
            "user@example.com;other@example.com",
        ];
        let params = Params::from_args(args).unwrap();
        let redacted = params.to_redacted_map();

        // Non-sensitive values should be preserved
        assert_eq!(redacted.get("antithesis.duration").unwrap(), "30");
        assert_eq!(
            redacted
                .get("antithesis.integrations.github.callback_url")
                .unwrap(),
            "https://example.com/callback"
        );

        // Sensitive values should be redacted
        assert_eq!(
            redacted
                .get("antithesis.integrations.github.token")
                .unwrap(),
            "[REDACTED]"
        );
        assert_eq!(
            redacted.get("antithesis.report.recipients").unwrap(),
            "[REDACTED]"
        );
    }
}
