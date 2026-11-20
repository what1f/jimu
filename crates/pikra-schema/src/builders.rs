//! Ergonomic JSON Schema builders and modifiers preserving key insertion order.

use serde_json::{Map, Number, Value};

use crate::Schema;

pub fn str_schema() -> Schema {
    type_schema("string")
}

pub fn int_schema() -> Schema {
    type_schema("integer")
}

pub fn num_schema() -> Schema {
    type_schema("number")
}

pub fn bool_schema() -> Schema {
    type_schema("boolean")
}

pub fn null_schema() -> Schema {
    type_schema("null")
}

pub fn const_schema(value: Value) -> Schema {
    let mut obj = Map::new();
    obj.insert("const".into(), value);
    Value::Object(obj)
}

pub fn ref_schema(pointer: impl Into<String>) -> Schema {
    let mut obj = Map::new();
    obj.insert("$ref".into(), Value::String(pointer.into()));
    Value::Object(obj)
}

pub fn arr_schema(items: Schema) -> Schema {
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String("array".into()));
    obj.insert("items".into(), items);
    Value::Object(obj)
}

pub fn tuple_schema(prefix_items: impl IntoIterator<Item = Schema>) -> Schema {
    let items: Vec<Schema> = prefix_items.into_iter().collect();
    let len = items.len();
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String("array".into()));
    obj.insert("prefixItems".into(), Value::Array(items));
    obj.insert("items".into(), Value::Bool(false));
    obj.insert("minItems".into(), Value::Number(Number::from(len)));
    Value::Object(obj)
}

pub fn map_schema(value_schema: Schema) -> Schema {
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String("object".into()));
    obj.insert("additionalProperties".into(), value_schema);
    Value::Object(obj)
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

pub fn string_enum(values: impl IntoIterator<Item = impl Into<String>>) -> Schema {
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String("string".into()));
    obj.insert(
        "enum".into(),
        Value::Array(values.into_iter().map(|v| Value::String(v.into())).collect()),
    );
    Value::Object(obj)
}

pub fn any_of(schemas: impl IntoIterator<Item = Schema>) -> Schema {
    let mut obj = Map::new();
    obj.insert("anyOf".into(), Value::Array(schemas.into_iter().collect()));
    Value::Object(obj)
}

pub fn one_of(schemas: impl IntoIterator<Item = Schema>) -> Schema {
    let mut obj = Map::new();
    obj.insert("oneOf".into(), Value::Array(schemas.into_iter().collect()));
    Value::Object(obj)
}

pub fn all_of(schemas: impl IntoIterator<Item = Schema>) -> Schema {
    let mut obj = Map::new();
    obj.insert("allOf".into(), Value::Array(schemas.into_iter().collect()));
    Value::Object(obj)
}

pub fn opt_nullable(schema: Schema) -> Schema {
    any_of([schema, null_schema()])
}

pub fn with_description(mut schema: Schema, description: impl Into<String>) -> Schema {
    if let Value::Object(ref mut map) = schema {
        map.insert("description".into(), Value::String(description.into()));
    }
    schema
}

pub fn with_title(mut schema: Schema, title: impl Into<String>) -> Schema {
    if let Value::Object(ref mut map) = schema {
        map.insert("title".into(), Value::String(title.into()));
    }
    schema
}

pub fn with_default(mut schema: Schema, default: Value) -> Schema {
    if let Value::Object(ref mut map) = schema {
        map.insert("default".into(), default);
    }
    schema
}

pub fn with_bounds(mut schema: Schema, min: Option<f64>, max: Option<f64>) -> Schema {
    if let Value::Object(ref mut map) = schema {
        if let Some(min) = min
            && let Some(num) = f64_to_json_number(min)
        {
            map.insert("minimum".into(), Value::Number(num));
        }
        if let Some(max) = max
            && let Some(num) = f64_to_json_number(max)
        {
            map.insert("maximum".into(), Value::Number(num));
        }
    }
    schema
}

pub fn with_exclusive_bounds(
    mut schema: Schema,
    exclusive_min: Option<f64>,
    exclusive_max: Option<f64>,
) -> Schema {
    if let Value::Object(ref mut map) = schema {
        if let Some(min) = exclusive_min
            && let Some(num) = f64_to_json_number(min)
        {
            map.insert("exclusiveMinimum".into(), Value::Number(num));
        }
        if let Some(max) = exclusive_max
            && let Some(num) = f64_to_json_number(max)
        {
            map.insert("exclusiveMaximum".into(), Value::Number(num));
        }
    }
    schema
}

pub fn with_length(mut schema: Schema, min: Option<usize>, max: Option<usize>) -> Schema {
    if let Value::Object(ref mut map) = schema {
        if let Some(min) = min {
            map.insert("minLength".into(), Value::Number(Number::from(min)));
        }
        if let Some(max) = max {
            map.insert("maxLength".into(), Value::Number(Number::from(max)));
        }
    }
    schema
}

pub fn with_items_bounds(mut schema: Schema, min: Option<usize>, max: Option<usize>) -> Schema {
    if let Value::Object(ref mut map) = schema {
        if let Some(min) = min {
            map.insert("minItems".into(), Value::Number(Number::from(min)));
        }
        if let Some(max) = max {
            map.insert("maxItems".into(), Value::Number(Number::from(max)));
        }
    }
    schema
}

pub fn with_unique_items(mut schema: Schema, unique: bool) -> Schema {
    if let Value::Object(ref mut map) = schema {
        map.insert("uniqueItems".into(), Value::Bool(unique));
    }
    schema
}

pub fn with_format(mut schema: Schema, format: impl Into<String>) -> Schema {
    if let Value::Object(ref mut map) = schema {
        map.insert("format".into(), Value::String(format.into()));
    }
    schema
}

pub fn with_additional_properties(mut schema: Schema, additional: Schema) -> Schema {
    if let Value::Object(ref mut map) = schema {
        map.insert("additionalProperties".into(), additional);
    }
    schema
}

pub fn with_defs(
    mut schema: Schema,
    defs: impl IntoIterator<Item = (impl Into<String>, Schema)>,
) -> Schema {
    if let Value::Object(ref mut map) = schema {
        let mut defs_map = Map::new();
        for (name, def_schema) in defs {
            defs_map.insert(name.into(), def_schema);
        }
        map.insert("$defs".into(), Value::Object(defs_map));
    }
    schema
}

fn type_schema(ty: &str) -> Schema {
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String(ty.into()));
    Value::Object(obj)
}

fn f64_to_json_number(n: f64) -> Option<Number> {
    if !n.is_finite() {
        return None;
    }
    if n.fract() == 0.0 && n >= (i64::MIN as f64) && n <= 9_007_199_254_740_991.0 {
        return Some(Number::from(n as i64));
    }
    Number::from_f64(n)
}
