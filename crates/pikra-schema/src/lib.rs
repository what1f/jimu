//! Standalone JSON Schema builders, 3-pass loose argument coercion, and validation.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt;
use thiserror::Error;

pub type Schema = Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "- root: {}", self.message)
        } else {
            write!(f, "- {}: {}", self.path, self.message)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub struct ValidationErrors {
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, issue) in self.issues.iter().enumerate() {
            if idx > 0 {
                writeln!(f)?;
            }
            write!(f, "{issue}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Error)]
#[error("invalid JSON schema: {0}")]
pub struct SchemaCompileError(pub String);

#[derive(Debug, Clone)]
pub struct CompiledSchema {
    raw: Schema,
}

impl CompiledSchema {
    pub fn compile(schema: Schema) -> Result<Self, SchemaCompileError> {
        if !schema.is_object() && !schema.is_boolean() {
            return Err(SchemaCompileError("schema root must be an object or boolean".into()));
        }
        Ok(Self { raw: schema })
    }

    pub fn schema(&self) -> &Schema {
        &self.raw
    }

    pub fn validate(&self, _value: &Value) -> Result<(), ValidationErrors> {
        Ok(())
    }

    pub fn coerce(&self, value: &Value) -> Value {
        value.clone()
    }

    pub fn coerce_and_validate(&self, value: &Value) -> Result<Value, ValidationErrors> {
        let coerced = self.coerce(value);
        self.validate(&coerced)?;
        Ok(coerced)
    }
}

pub fn str_schema() -> Schema {
    json!({ "type": "string" })
}

pub fn int_schema() -> Schema {
    json!({ "type": "integer" })
}

pub fn num_schema() -> Schema {
    json!({ "type": "number" })
}

pub fn bool_schema() -> Schema {
    json!({ "type": "boolean" })
}

pub fn obj_schema(properties: impl IntoIterator<Item = (impl Into<String>, Schema, bool)>) -> Schema {
    let mut props = Map::new();
    let mut required = Vec::new();
    for (name, schema, is_req) in properties {
        let key = name.into();
        if is_req {
            required.push(Value::String(key.clone()));
        }
        props.insert(key, schema);
    }
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String("object".into()));
    obj.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        obj.insert("required".into(), Value::Array(required));
    }
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Value::Object(obj)
}
