use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityErrorKind {
    InvalidInput,
    InvalidOutput,
    InvalidConfiguration,
    DependencyUnavailable,
    Unavailable,
    OperationFailed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityError {
    pub kind: CapabilityErrorKind,
    pub message: String,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CapabilityError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDependency {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRequirement {
    pub permission: String,
    pub description: String,
    pub sensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Available,
    Disabled,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityEffect {
    ReadOnly,
    #[default]
    Mutating,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub input_schema: Value,
    pub output_schema: Value,
    #[serde(default)]
    pub configuration_schema: Option<Value>,
    #[serde(default)]
    pub effect: CapabilityEffect,
    #[serde(default)]
    pub effects: Vec<String>,
    pub operations: Vec<String>,
    pub state: CapabilityState,
    #[serde(default)]
    pub dependencies: Vec<CapabilityDependency>,
    #[serde(default)]
    pub requirements: Vec<ExecutionRequirement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequest {
    pub capability_id: String,
    pub capability_version: String,
    pub operation: String,
    pub input: Value,
}

impl CapabilityDescriptor {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.id, "capability identifier")?;
        validate_version(&self.version)?;
        if self.name.trim().is_empty() || self.description.trim().is_empty() {
            bail!("Capability name and description must not be empty");
        }
        if self.effects.is_empty() || self.effects.iter().any(|effect| effect.trim().is_empty()) {
            bail!("Capabilities must declare non-empty effect descriptions");
        }
        if self.operations.is_empty() {
            bail!("Capabilities must declare at least one operation");
        }
        let mut operations = BTreeSet::new();
        for operation in &self.operations {
            validate_identifier(operation, "operation identifier")?;
            if !operations.insert(operation) {
                bail!("Capability operations must not contain duplicates");
            }
        }
        validate_schema(&self.input_schema).context("Invalid capability input schema")?;
        validate_schema(&self.output_schema).context("Invalid capability output schema")?;
        if let Some(schema) = &self.configuration_schema {
            validate_schema(schema).context("Invalid capability configuration schema")?;
        }
        for dependency in &self.dependencies {
            validate_identifier(&dependency.id, "dependency identifier")?;
            validate_version(&dependency.version)?;
        }
        for requirement in &self.requirements {
            validate_identifier(&requirement.permission, "permission identifier")?;
            if requirement.description.trim().is_empty() {
                bail!("Permission requirement descriptions must not be empty");
            }
        }
        Ok(())
    }

    pub fn validate_input(&self, input: &Value) -> std::result::Result<(), CapabilityError> {
        validate_value(&self.input_schema, input, "$").map_err(|error| CapabilityError {
            kind: CapabilityErrorKind::InvalidInput,
            message: error.to_string(),
        })
    }

    pub fn validate_output(&self, output: &Value) -> std::result::Result<(), CapabilityError> {
        validate_value(&self.output_schema, output, "$").map_err(|error| CapabilityError {
            kind: CapabilityErrorKind::InvalidOutput,
            message: error.to_string(),
        })
    }

    pub fn validate_configuration(
        &self,
        configuration: &Value,
    ) -> std::result::Result<(), CapabilityError> {
        match &self.configuration_schema {
            Some(schema) => {
                validate_value(schema, configuration, "$").map_err(|error| CapabilityError {
                    kind: CapabilityErrorKind::InvalidConfiguration,
                    message: error.to_string(),
                })
            }
            None if configuration.is_null() => Ok(()),
            None => Err(CapabilityError {
                kind: CapabilityErrorKind::InvalidConfiguration,
                message: format!("Capability '{}' does not accept configuration", self.id),
            }),
        }
    }
}

#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    capabilities: BTreeMap<String, CapabilityDescriptor>,
}

impl CapabilityRegistry {
    pub fn register(&mut self, descriptor: CapabilityDescriptor) -> Result<()> {
        descriptor.validate()?;
        if self.capabilities.contains_key(&descriptor.id) {
            bail!("Capability '{}' is already registered", descriptor.id);
        }
        self.capabilities.insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&CapabilityDescriptor> {
        self.capabilities.get(id)
    }

    pub fn discover(&self) -> impl Iterator<Item = &CapabilityDescriptor> {
        self.capabilities.values()
    }

    pub fn set_state(&mut self, id: &str, state: CapabilityState) -> Result<()> {
        let descriptor = self
            .capabilities
            .get_mut(id)
            .with_context(|| format!("Capability '{id}' is not registered"))?;
        descriptor.state = state;
        Ok(())
    }
}

pub fn validate_identifier(value: &str, label: &str) -> Result<()> {
    let mut chars = value.chars();
    let valid_first = chars
        .next()
        .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit());
    if !valid_first
        || value.len() > 128
        || !chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        })
    {
        bail!("Invalid {label}; use 1-128 lowercase letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

pub fn validate_version(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        bail!("Capability versions must use numeric major.minor.patch format");
    }
    Ok(())
}

fn validate_schema(schema: &Value) -> Result<()> {
    let object = schema.as_object().context("schema must be a JSON object")?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type"
                | "description"
                | "enum"
                | "properties"
                | "required"
                | "items"
                | "additionalProperties"
        ) {
            bail!("unsupported schema keyword '{key}'");
        }
    }
    let schema_type = object
        .get("type")
        .and_then(Value::as_str)
        .context("schema must declare a string 'type'")?;
    if !matches!(
        schema_type,
        "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
    ) {
        bail!("unsupported schema type '{schema_type}'");
    }
    if let Some(description) = object.get("description")
        && !description.is_string()
    {
        bail!("schema description must be a string");
    }
    if let Some(values) = object.get("enum")
        && (!values.is_array() || values.as_array().is_some_and(Vec::is_empty))
    {
        bail!("schema enum must be a non-empty array");
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .context("schema properties must be an object")?;
        for property in properties.values() {
            validate_schema(property)?;
        }
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .context("schema required must be an array")?;
        if required.iter().any(|property| !property.is_string()) {
            bail!("schema required entries must be strings");
        }
    }
    if let Some(items) = object.get("items") {
        validate_schema(items)?;
    }
    if let Some(additional) = object.get("additionalProperties")
        && !additional.is_boolean()
    {
        bail!("schema additionalProperties must be a boolean");
    }
    Ok(())
}

fn validate_value(schema: &Value, value: &Value, path: &str) -> Result<()> {
    let schema_type = schema["type"]
        .as_str()
        .context("Capability schema is missing a type")?;
    let type_matches = match schema_type {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    };
    if !type_matches {
        bail!("Capability value at {path} does not match schema type '{schema_type}'");
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        bail!("Capability value at {path} is not an allowed enum value");
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for property in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(property) {
                    bail!("Capability value at {path} is missing required property '{property}'");
                }
            }
        }
        let properties = schema["properties"].as_object();
        if schema["additionalProperties"] == Value::Bool(false)
            && let Some(key) = object
                .keys()
                .find(|key| properties.is_none_or(|properties| !properties.contains_key(*key)))
        {
            bail!("Capability value at {path} contains unsupported property '{key}'");
        }
        if let Some(properties) = properties {
            for (key, property_schema) in properties {
                if let Some(property_value) = object.get(key) {
                    validate_value(property_schema, property_value, &format!("{path}.{key}"))?;
                }
            }
        }
    }
    if let Some(items) = value.as_array()
        && let Some(item_schema) = schema.get("items")
    {
        for (index, item) in items.iter().enumerate() {
            validate_value(item_schema, item, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor(id: &str) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: id.to_owned(),
            name: "Example".to_owned(),
            description: "Test-only capability descriptor".to_owned(),
            version: "1.0.0".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }),
            output_schema: json!({"type": "string"}),
            configuration_schema: None,
            effect: CapabilityEffect::ReadOnly,
            effects: vec!["Reads only the provided text.".to_owned()],
            operations: vec!["echo".to_owned()],
            state: CapabilityState::Available,
            dependencies: vec![CapabilityDependency {
                id: "core.storage".to_owned(),
                version: "1.0.0".to_owned(),
            }],
            requirements: vec![ExecutionRequirement {
                permission: "data.read".to_owned(),
                description: "Read a bounded data source".to_owned(),
                sensitive: false,
            }],
        }
    }

    #[test]
    fn registry_discovers_capabilities_and_rejects_duplicate_ids() -> Result<()> {
        let mut registry = CapabilityRegistry::default();
        registry.register(descriptor("test.echo"))?;
        assert_eq!(registry.discover().count(), 1);
        assert_eq!(
            registry
                .get("test.echo")
                .map(|capability| capability.version.as_str()),
            Some("1.0.0")
        );
        assert!(registry.register(descriptor("test.echo")).is_err());
        registry.set_state("test.echo", CapabilityState::Disabled)?;
        assert_eq!(
            registry.get("test.echo").map(|capability| capability.state),
            Some(CapabilityState::Disabled)
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_descriptor_and_schema() {
        let mut invalid_id = descriptor("Test.Echo");
        assert!(invalid_id.validate().is_err());
        invalid_id.id = "test.echo".to_owned();
        invalid_id.version = "v1".to_owned();
        assert!(invalid_id.validate().is_err());
        invalid_id.version = "1.0.0".to_owned();
        invalid_id.input_schema = json!({"type": "object", "patternProperties": {}});
        assert!(invalid_id.validate().is_err());
    }

    #[test]
    fn validates_input_output_and_configuration_schemas() -> Result<()> {
        let mut capability = descriptor("test.echo");
        capability.configuration_schema = Some(json!({
            "type": "object",
            "properties": {"endpoint": {"type": "string"}},
            "required": ["endpoint"],
            "additionalProperties": false
        }));
        capability.validate()?;
        capability.validate_input(&json!({"text": "hello"}))?;
        capability.validate_output(&json!("hello"))?;
        capability.validate_configuration(&json!({"endpoint": "local"}))?;
        assert_eq!(
            capability
                .validate_input(&json!({"text": 4}))
                .expect_err("wrong input type")
                .kind,
            CapabilityErrorKind::InvalidInput
        );
        assert!(
            capability
                .validate_input(&json!({"text": "ok", "extra": true}))
                .is_err()
        );
        assert_eq!(
            capability
                .validate_configuration(&Value::Null)
                .expect_err("missing configuration")
                .kind,
            CapabilityErrorKind::InvalidConfiguration
        );
        Ok(())
    }
}
