use jsonschema::Validator;
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
    /// Arguments should be in the format: `--key.path value`
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

    /// Get the inner map.
    pub fn into_inner(self) -> Map<String, Value> {
        self.inner
    }

    /// Get a reference to the inner map.
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.inner
    }

    /// Convert to a JSON value.
    pub fn to_value(&self) -> Value {
        Value::Object(self.inner.clone())
    }
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
            let value = parse_value(value.as_ref());

            // Handle nested keys like "antithesis.integrations.github"
            insert_nested(&mut map, key, value)?;
        } else {
            return Err(Error::InvalidArgs(format!(
                "unexpected argument: {} (expected --key)",
                arg
            )));
        }
    }

    Ok(map)
}

fn insert_nested(map: &mut Map<String, Value>, key: &str, value: Value) -> Result<()> {
    // Check if this is a nested integration key
    if let Some(rest) = key.strip_prefix("antithesis.integrations.") {
        // Parse: antithesis.integrations.{provider}.{field}
        if let Some((provider, field)) = rest.split_once('.') {
            let integration_key = format!("antithesis.integrations.{}", provider);

            let integration = map
                .entry(&integration_key)
                .or_insert_with(|| Value::Object(Map::new()));

            if let Value::Object(obj) = integration {
                obj.insert(field.to_string(), value);
            }
            return Ok(());
        }
    }

    // Flat key
    map.insert(key.to_string(), value);
    Ok(())
}

fn parse_value(s: &str) -> Value {
    Value::String(s.to_string())
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
        return Err(Error::ValidationFailed(errors));
    }

    Ok(())
}

/// Get the raw schema as a JSON value.
pub fn schema() -> Value {
    serde_json::from_str(SCHEMA).expect("valid schema")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_args() {
        let args = ["--antithesis.duration", "30", "--antithesis.description", "test run"];
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

        let github = params
            .as_map()
            .get("antithesis.integrations.github")
            .unwrap();
        assert_eq!(github["callback_url"], "https://github.com/cb");
        assert_eq!(github["token"], "secret");
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
}
