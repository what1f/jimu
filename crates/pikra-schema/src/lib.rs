//! Standalone JSON Schema builders, 3-pass loose argument coercion, and validation.

mod builders;
mod coerce;
mod formats;
mod pattern;
mod resolve;
mod validate;

pub use builders::{
    all_of, any_of, arr_schema, bool_schema, const_schema, int_schema, map_schema, null_schema,
    num_schema, obj_schema, one_of, opt_nullable, ref_schema, str_schema, string_enum,
    tuple_schema, with_additional_properties, with_bounds, with_default, with_defs,
    with_description, with_exclusive_bounds, with_format, with_items_bounds, with_length,
    with_title, with_unique_items,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

impl std::error::Error for ValidationErrors {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCompileError(pub String);

impl fmt::Display for SchemaCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON schema: {}", self.0)
    }
}

impl std::error::Error for SchemaCompileError {}

#[derive(Debug, Clone)]
pub struct CompiledSchema {
    raw: Schema,
}

impl CompiledSchema {
    pub fn compile(schema: Schema) -> Result<Self, SchemaCompileError> {
        if !schema.is_object() && !schema.is_boolean() {
            return Err(SchemaCompileError(
                "schema root must be an object or boolean".into(),
            ));
        }
        resolve::validate_schema_tree(&schema, &schema, "#")?;
        Ok(Self { raw: schema })
    }

    pub fn schema(&self) -> &Schema {
        &self.raw
    }

    pub fn is_valid(&self, value: &Value) -> bool {
        validate::passes_node(&self.raw, &self.raw, value)
    }

    pub fn validate(&self, value: &Value) -> Result<(), ValidationErrors> {
        validate::validate_root(&self.raw, value)
    }

    pub fn coerce(&self, value: &Value) -> Value {
        coerce::coerce_root(&self.raw, value)
    }

    pub fn coerce_and_validate(&self, value: &Value) -> Result<Value, ValidationErrors> {
        let coerced = self.coerce(value);
        self.validate(&coerced)?;
        Ok(coerced)
    }
}
