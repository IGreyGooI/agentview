//! Provider-neutral native tool metadata.
//!
//! A tool definition is authored once by a Component and carried unchanged
//! through the Frame protocol. Provider adapters lower this neutral shape to
//! their own request DTOs.

use serde_json::Value;

const LEGACY_NATIVE_TOOL_DESCRIPTION: &str = "AgentView native tool";

/// Complete provider-neutral declaration of one native function tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    name: String,
    description: String,
    input_schema: Value,
    strict: bool,
}

impl ToolDefinition {
    /// Builds a function tool definition after validating the portable JSON
    /// values that will be included in a canonical Frame.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Result<Self, ToolDefinitionError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ToolDefinitionError::EmptyName);
        }
        if !input_schema.is_object() {
            return Err(ToolDefinitionError::InputSchemaMustBeObject);
        }
        if input_schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(ToolDefinitionError::InputSchemaRootTypeMustBeObject);
        }
        validate_i_json_value(&input_schema)?;
        Ok(Self {
            name,
            description: description.into(),
            input_schema,
            strict: false,
        })
    }

    /// Selects strict provider-side schema handling for adapters that support
    /// it. Native declarations remain permissive by default for compatibility.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    pub const fn strict(&self) -> bool {
        self.strict
    }

    /// Preserves the historical `NativeToolCall::named` contract: an open
    /// object schema and the framework supplied description.
    pub(crate) fn legacy_name_only(name: impl Into<String>) -> Result<Self, ToolDefinitionError> {
        Self::new(
            name,
            LEGACY_NATIVE_TOOL_DESCRIPTION,
            serde_json::json!({
                "type": "object",
                "additionalProperties": true,
            }),
        )
    }
}

/// Rejection reason for a provider-neutral tool declaration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ToolDefinitionError {
    #[error("native tool name must not be empty")]
    EmptyName,
    #[error("native tool input schema must be a JSON object")]
    InputSchemaMustBeObject,
    #[error("native tool input schema root type must be `object`")]
    InputSchemaRootTypeMustBeObject,
    #[error("native tool input schema contains non-I-JSON number `{value}`")]
    NonIJsonNumber { value: String },
}

pub(crate) fn validate_i_json_value(value: &Value) -> Result<(), ToolDefinitionError> {
    const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
    match value {
        Value::Number(number) => {
            let valid = if let Some(value) = number.as_i64() {
                value.unsigned_abs() <= MAX_SAFE_INTEGER
            } else if let Some(value) = number.as_u64() {
                value <= MAX_SAFE_INTEGER
            } else {
                number.as_f64().is_some_and(f64::is_finite)
            };
            if !valid {
                return Err(ToolDefinitionError::NonIJsonNumber {
                    value: number.to_string(),
                });
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_i_json_value(value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_i_json_value(value)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ToolDefinition, ToolDefinitionError};

    #[test]
    fn definition_requires_an_object_i_json_schema() {
        assert_eq!(
            ToolDefinition::new("", "description", json!({})),
            Err(ToolDefinitionError::EmptyName)
        );
        assert_eq!(
            ToolDefinition::new("lookup", "description", json!([])),
            Err(ToolDefinitionError::InputSchemaMustBeObject)
        );
        assert_eq!(
            ToolDefinition::new("lookup", "description", json!({})),
            Err(ToolDefinitionError::InputSchemaRootTypeMustBeObject)
        );
        assert_eq!(
            ToolDefinition::new(
                "lookup",
                "description",
                json!({"type": "object", "maximum": 9_007_199_254_740_992_u64})
            ),
            Err(ToolDefinitionError::NonIJsonNumber {
                value: "9007199254740992".to_owned(),
            })
        );
    }

    #[test]
    fn strict_defaults_to_false_and_is_configurable() {
        let definition =
            ToolDefinition::new("lookup", "description", json!({"type": "object"})).unwrap();
        assert!(!definition.strict());
        assert!(definition.with_strict(true).strict());
    }
}
