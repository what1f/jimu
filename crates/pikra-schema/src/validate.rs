//! Full JSON Schema validator with cycle protection and model-friendly diagnostics.

use std::cmp::Ordering;
use std::collections::HashSet;

use serde_json::{Map, Number, Value};

use crate::formats::check_format;
use crate::resolve::resolve_pointer;
use crate::{ValidationErrors, ValidationIssue};

const MAX_LISTED_ENUM_VALUES: usize = 20;

pub(crate) fn validate_root(root: &Value, value: &Value) -> Result<(), ValidationErrors> {
    if passes_node(root, root, value) {
        return Ok(());
    }
    let mut issues = Vec::new();
    let mut ctx = ValidateCtx::new(root);
    ctx.walk(root, value, "", &mut issues);
    if issues.is_empty() {
        issues.push(ValidationIssue {
            path: String::new(),
            message: "invalid value".into(),
        });
    }
    Err(ValidationErrors { issues })
}

pub(crate) fn passes_node(root: &Value, node: &Value, value: &Value) -> bool {
    let mut ctx = ValidateCtx::new(root);
    ctx.check(node, value)
}

struct ValidateCtx<'a> {
    root: &'a Value,
    /// Cycle guard tracking active `(schema_node_ptr, value_ptr)` pairs.
    active: Vec<(usize, usize)>,
}

impl<'a> ValidateCtx<'a> {
    fn new(root: &'a Value) -> Self {
        Self {
            root,
            active: Vec::new(),
        }
    }

    fn key_pair(node: &Value, value: &Value) -> (usize, usize) {
        (node as *const Value as usize, value as *const Value as usize)
    }

    fn check(&mut self, node: &'a Value, value: &Value) -> bool {
        match node {
            Value::Bool(b) => return *b,
            Value::Object(_) => {}
            _ => return true,
        }
        let pair = Self::key_pair(node, value);
        if self.active.contains(&pair) {
            return false;
        }
        self.active.push(pair);
        let ok = self.check_inner(node, value);
        self.active.pop();
        ok
    }

    fn check_inner(&mut self, node: &'a Value, value: &Value) -> bool {
        let Some(map) = node.as_object() else {
            return true;
        };

        if let Some(ref_val) = map.get("$ref").and_then(Value::as_str) {
            let Ok(target) = resolve_pointer(self.root, ref_val) else {
                return false;
            };
            if !self.check(target, value) {
                return false;
            }
        }

        if let Some(ty) = map.get("type")
            && !matches_type_keyword(value, ty)
        {
            return false;
        }

        if let Some(const_val) = map.get("const")
            && !json_equal(value, const_val)
        {
            return false;
        }

        if let Some(enum_vals) = map.get("enum").and_then(Value::as_array)
            && !enum_vals.iter().any(|candidate| json_equal(value, candidate))
        {
            return false;
        }

        match value {
            Value::String(s) if !check_string(map, s) => return false,
            Value::Number(n) if !check_number(map, n) => return false,
            Value::Object(obj) if !self.check_object(map, obj) => return false,
            Value::Array(arr) if !self.check_array(map, arr) => return false,
            _ => {}
        }

        if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
            for sub in all_of {
                if !self.check(sub, value) {
                    return false;
                }
            }
        }

        if let Some(any_of) = map.get("anyOf").and_then(Value::as_array)
            && !any_of.iter().any(|sub| self.check(sub, value))
        {
            return false;
        }

        if let Some(one_of) = map.get("oneOf").and_then(Value::as_array) {
            let mut matches = 0usize;
            for sub in one_of {
                if self.check(sub, value) {
                    matches += 1;
                    if matches > 1 {
                        return false;
                    }
                }
            }
            if matches != 1 {
                return false;
            }
        }

        if let Some(not_schema) = map.get("not")
            && self.check(not_schema, value)
        {
            return false;
        }

        true
    }

    fn check_object(&mut self, schema: &'a Map<String, Value>, obj: &Map<String, Value>) -> bool {
        if let Some(min) = schema.get("minProperties").and_then(as_usize)
            && obj.len() < min
        {
            return false;
        }
        if let Some(max) = schema.get("maxProperties").and_then(as_usize)
            && obj.len() > max
        {
            return false;
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for req in required {
                if let Some(key) = req.as_str()
                    && !obj.contains_key(key)
                {
                    return false;
                }
            }
        }

        let properties = schema.get("properties").and_then(Value::as_object);
        let additional = schema.get("additionalProperties");

        for (key, val) in obj {
            if let Some(prop_schema) = properties.and_then(|props| props.get(key)) {
                if !self.check(prop_schema, val) {
                    return false;
                }
            } else if let Some(addl) = additional
                && !self.check(addl, val)
            {
                return false;
            }
        }
        true
    }

    fn check_array(&mut self, schema: &'a Map<String, Value>, arr: &[Value]) -> bool {
        if let Some(min) = schema.get("minItems").and_then(as_usize)
            && arr.len() < min
        {
            return false;
        }
        if let Some(max) = schema.get("maxItems").and_then(as_usize)
            && arr.len() > max
        {
            return false;
        }
        if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true)
            && !has_unique_items(arr)
        {
            return false;
        }

        for (idx, item) in arr.iter().enumerate() {
            if let Some(item_schema) = item_schema_at(schema, idx)
                && !self.check(item_schema, item)
            {
                return false;
            }
        }
        true
    }

    fn walk(
        &mut self,
        node: &'a Value,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationIssue>,
    ) {
        match node {
            Value::Bool(true) => return,
            Value::Bool(false) => {
                out.push(ValidationIssue {
                    path: path.to_string(),
                    message: "no value is allowed here".into(),
                });
                return;
            }
            Value::Object(_) => {}
            _ => return,
        }

        if self.check(node, value) {
            return;
        }

        let pair = Self::key_pair(node, value);
        if self.active.contains(&pair) {
            return;
        }
        self.active.push(pair);
        self.walk_inner(node, value, path, out);
        self.active.pop();
    }

    fn walk_inner(
        &mut self,
        node: &'a Value,
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationIssue>,
    ) {
        let Some(map) = node.as_object() else {
            return;
        };

        if let Some(ty) = map.get("type")
            && !matches_type_keyword(value, ty)
        {
            let expected = format_expected_types(ty);
            let actual = json_type_name(value);
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("expected {expected}, got {actual}"),
            });
            return;
        }

        if let Some(const_val) = map.get("const")
            && !json_equal(value, const_val)
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must equal {}", const_val),
            });
        }

        if let Some(enum_vals) = map.get("enum").and_then(Value::as_array)
            && !enum_vals.iter().any(|candidate| json_equal(value, candidate))
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must be one of {}", format_value_list(enum_vals)),
            });
        }

        match value {
            Value::String(s) => walk_string(map, s, path, out),
            Value::Number(n) => walk_number(map, n, path, out),
            Value::Object(obj) => self.walk_object(map, obj, path, out),
            Value::Array(arr) => self.walk_array(map, arr, path, out),
            _ => {}
        }

        if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
            for sub in all_of {
                self.walk(sub, value, path, out);
            }
        }

        if let Some(any_of) = map.get("anyOf").and_then(Value::as_array) {
            self.walk_union(any_of, value, path, out, false);
        }

        if let Some(one_of) = map.get("oneOf").and_then(Value::as_array) {
            self.walk_union(one_of, value, path, out, true);
        }

        if let Some(not_schema) = map.get("not")
            && self.check(not_schema, value)
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: "must not match disallowed schema".into(),
            });
        }

        if let Some(ref_val) = map.get("$ref").and_then(Value::as_str)
            && let Ok(target) = resolve_pointer(self.root, ref_val)
        {
            self.walk(target, value, path, out);
        }
    }

    fn walk_object(
        &mut self,
        schema: &'a Map<String, Value>,
        obj: &Map<String, Value>,
        path: &str,
        out: &mut Vec<ValidationIssue>,
    ) {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for req in required {
                if let Some(key) = req.as_str()
                    && !obj.contains_key(key)
                {
                    out.push(ValidationIssue {
                        path: join_field(path, key),
                        message: "missing".into(),
                    });
                }
            }
        }

        let properties = schema.get("properties").and_then(Value::as_object);
        let additional = schema.get("additionalProperties");

        for (key, val) in obj {
            let child_path = join_field(path, key);
            if let Some(prop_schema) = properties.and_then(|props| props.get(key)) {
                self.walk(prop_schema, val, &child_path, out);
            } else if let Some(addl) = additional {
                if addl == &Value::Bool(false) {
                    out.push(ValidationIssue {
                        path: child_path,
                        message: "unexpected property".into(),
                    });
                } else {
                    self.walk(addl, val, &child_path, out);
                }
            }
        }

        if let Some(min) = schema.get("minProperties").and_then(as_usize)
            && obj.len() < min
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must have at least {min} properties"),
            });
        }
        if let Some(max) = schema.get("maxProperties").and_then(as_usize)
            && obj.len() > max
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must have at most {max} properties"),
            });
        }
    }

    fn walk_array(
        &mut self,
        schema: &'a Map<String, Value>,
        arr: &[Value],
        path: &str,
        out: &mut Vec<ValidationIssue>,
    ) {
        for (idx, item) in arr.iter().enumerate() {
            if let Some(item_schema) = item_schema_at(schema, idx) {
                let item_path = join_index(path, idx);
                self.walk(item_schema, item, &item_path, out);
            }
        }
        if let Some(min) = schema.get("minItems").and_then(as_usize)
            && arr.len() < min
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must have at least {min} items"),
            });
        }
        if let Some(max) = schema.get("maxItems").and_then(as_usize)
            && arr.len() > max
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must have at most {max} items"),
            });
        }
        if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true)
            && !has_unique_items(arr)
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: "items must be unique".into(),
            });
        }
    }

    fn walk_union(
        &mut self,
        branches: &'a [Value],
        value: &Value,
        path: &str,
        out: &mut Vec<ValidationIssue>,
        exactly_one: bool,
    ) {
        let match_count = branches.iter().filter(|b| self.check(b, value)).count();
        if match_count == 1 || (!exactly_one && match_count > 0) {
            return;
        }
        if match_count > 1 {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: "matches more than one alternative".into(),
            });
            return;
        }

        // Tagged union: blame the branch selected by the discriminator const property.
        if let Some(obj) = value.as_object()
            && let Some((tag_key, tag_values)) = self.find_discriminator(branches)
        {
            if let Some(actual_tag) = obj.get(&tag_key)
                && let Some(idx) = tag_values.iter().position(|v| json_equal(v, actual_tag))
            {
                self.walk(&branches[idx], value, path, out);
                return;
            }
            out.push(ValidationIssue {
                path: join_field(path, &tag_key),
                message: format!("must be one of {}", format_value_list(&tag_values)),
            });
            return;
        }

        // Otherwise pick the branch that declares the most keys of `value`, breaking ties by
        // the deepest first error path.
        let mut best: Option<(usize, usize, Vec<ValidationIssue>)> = None;
        for branch in branches {
            let mut branch_issues = Vec::new();
            self.walk(branch, value, path, &mut branch_issues);
            if branch_issues.is_empty() {
                continue;
            }
            let declared = value
                .as_object()
                .map(|obj| self.declared_key_overlap(branch, obj))
                .unwrap_or(0);
            let depth = branch_issues[0].path.len();
            let better = match &best {
                None => true,
                Some((best_decl, best_depth, _)) => {
                    declared > *best_decl || (declared == *best_decl && depth > *best_depth)
                }
            };
            if better {
                best = Some((declared, depth, branch_issues));
            }
        }

        if let Some((_, _, issues)) = best {
            out.extend(issues);
        } else {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: "matches no alternative".into(),
            });
        }
    }

    fn resolve_if_ref(&self, node: &'a Value) -> &'a Value {
        if let Some(ref_str) = node.as_object().and_then(|m| m.get("$ref")).and_then(Value::as_str)
            && let Ok(target) = resolve_pointer(self.root, ref_str)
        {
            return target;
        }
        node
    }

    fn declared_key_overlap(&self, branch: &'a Value, obj: &Map<String, Value>) -> usize {
        let resolved = self.resolve_if_ref(branch);
        let Some(props) = resolved
            .as_object()
            .and_then(|m| m.get("properties"))
            .and_then(Value::as_object)
        else {
            return 0;
        };
        obj.keys().filter(|k| props.contains_key(*k)).count()
    }

    fn find_discriminator(&self, branches: &'a [Value]) -> Option<(String, Vec<Value>)> {
        if branches.len() < 2 {
            return None;
        }
        let resolved: Vec<&Value> = branches.iter().map(|b| self.resolve_if_ref(b)).collect();
        let first_props = resolved[0]
            .as_object()
            .and_then(|m| m.get("properties"))
            .and_then(Value::as_object)?;

        for key in first_props.keys() {
            let mut tags = Vec::with_capacity(resolved.len());
            let mut valid = true;
            for branch in &resolved {
                let tag = branch
                    .as_object()
                    .and_then(|m| m.get("properties"))
                    .and_then(Value::as_object)
                    .and_then(|p| p.get(key))
                    .and_then(Value::as_object)
                    .and_then(|p| p.get("const"));
                match tag {
                    Some(v) if !v.is_object() && !v.is_array() => {
                        if tags.iter().any(|prev| json_equal(prev, v)) {
                            valid = false;
                            break;
                        }
                        tags.push(v.clone());
                    }
                    _ => {
                        valid = false;
                        break;
                    }
                }
            }
            if valid {
                return Some((key.clone(), tags));
            }
        }
        None
    }
}

fn check_string(schema: &Map<String, Value>, s: &str) -> bool {
    let min_len = schema.get("minLength").and_then(as_usize);
    let max_len = schema.get("maxLength").and_then(as_usize);
    if min_len.is_some() || max_len.is_some() {
        let char_count = s.chars().count();
        if let Some(min) = min_len
            && char_count < min
        {
            return false;
        }
        if let Some(max) = max_len
            && char_count > max
        {
            return false;
        }
    }
    if let Some(fmt) = schema.get("format").and_then(Value::as_str)
        && !check_format(fmt, s)
    {
        return false;
    }
    true
}

fn walk_string(
    schema: &Map<String, Value>,
    s: &str,
    path: &str,
    out: &mut Vec<ValidationIssue>,
) {
    let min_len = schema.get("minLength").and_then(as_usize);
    let max_len = schema.get("maxLength").and_then(as_usize);
    if min_len.is_some() || max_len.is_some() {
        let char_count = s.chars().count();
        if let Some(min) = min_len
            && char_count < min
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must be at least {min} characters"),
            });
        }
        if let Some(max) = max_len
            && char_count > max
        {
            out.push(ValidationIssue {
                path: path.to_string(),
                message: format!("must be at most {max} characters"),
            });
        }
    }
    if let Some(fmt) = schema.get("format").and_then(Value::as_str)
        && !check_format(fmt, s)
    {
        out.push(ValidationIssue {
            path: path.to_string(),
            message: format!("must be a valid {fmt}"),
        });
    }
}

fn check_number(schema: &Map<String, Value>, n: &Number) -> bool {
    if let Some(min) = schema.get("minimum").and_then(Value::as_number)
        && cmp_numbers(n, min) == Ordering::Less
    {
        return false;
    }
    if let Some(max) = schema.get("maximum").and_then(Value::as_number)
        && cmp_numbers(n, max) == Ordering::Greater
    {
        return false;
    }
    if let Some(xmin) = schema.get("exclusiveMinimum").and_then(Value::as_number)
        && cmp_numbers(n, xmin) != Ordering::Greater
    {
        return false;
    }
    if let Some(xmax) = schema.get("exclusiveMaximum").and_then(Value::as_number)
        && cmp_numbers(n, xmax) != Ordering::Less
    {
        return false;
    }
    if let Some(mult) = schema.get("multipleOf").and_then(Value::as_number)
        && !is_multiple_of(n, mult)
    {
        return false;
    }
    true
}

fn walk_number(
    schema: &Map<String, Value>,
    n: &Number,
    path: &str,
    out: &mut Vec<ValidationIssue>,
) {
    if let Some(min) = schema.get("minimum").and_then(Value::as_number)
        && cmp_numbers(n, min) == Ordering::Less
    {
        out.push(ValidationIssue {
            path: path.to_string(),
            message: format!("must be >= {min}"),
        });
    }
    if let Some(max) = schema.get("maximum").and_then(Value::as_number)
        && cmp_numbers(n, max) == Ordering::Greater
    {
        out.push(ValidationIssue {
            path: path.to_string(),
            message: format!("must be <= {max}"),
        });
    }
    if let Some(xmin) = schema.get("exclusiveMinimum").and_then(Value::as_number)
        && cmp_numbers(n, xmin) != Ordering::Greater
    {
        out.push(ValidationIssue {
            path: path.to_string(),
            message: format!("must be > {xmin}"),
        });
    }
    if let Some(xmax) = schema.get("exclusiveMaximum").and_then(Value::as_number)
        && cmp_numbers(n, xmax) != Ordering::Less
    {
        out.push(ValidationIssue {
            path: path.to_string(),
            message: format!("must be < {xmax}"),
        });
    }
    if let Some(mult) = schema.get("multipleOf").and_then(Value::as_number)
        && !is_multiple_of(n, mult)
    {
        out.push(ValidationIssue {
            path: path.to_string(),
            message: format!("must be a multiple of {mult}"),
        });
    }
}

pub(crate) fn item_schema_at(schema: &Map<String, Value>, index: usize) -> Option<&Value> {
    if let Some(items_arr) = schema.get("items").and_then(Value::as_array) {
        return items_arr
            .get(index)
            .or_else(|| schema.get("additionalItems"));
    }
    if let Some(prefix) = schema.get("prefixItems").and_then(Value::as_array)
        && index < prefix.len()
    {
        return Some(&prefix[index]);
    }
    schema.get("items")
}

pub(crate) fn matches_type(value: &Value, ty: &str) -> bool {
    match ty {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(is_json_integer),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn matches_type_keyword(value: &Value, ty: &Value) -> bool {
    match ty {
        Value::String(s) => matches_type(value, s),
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .any(|t| matches_type(value, t)),
        _ => true,
    }
}

pub(crate) fn is_json_integer(n: &Number) -> bool {
    if n.is_i64() || n.is_u64() {
        return true;
    }
    n.as_f64()
        .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

pub(crate) fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => cmp_numbers(x, y) == Ordering::Equal,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| json_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            if x.len() != y.len() {
                return false;
            }
            x.iter()
                .all(|(k, v)| y.get(k).is_some_and(|yv| json_equal(v, yv)))
        }
        _ => false,
    }
}

fn has_unique_items(arr: &[Value]) -> bool {
    // Fast path for arrays of strings (common in tool parameters).
    if arr.len() > 8 && arr.iter().all(Value::is_string) {
        let mut seen = HashSet::with_capacity(arr.len());
        for item in arr {
            if let Some(s) = item.as_str()
                && !seen.insert(s)
            {
                return false;
            }
        }
        return true;
    }
    for i in 1..arr.len() {
        for j in 0..i {
            if json_equal(&arr[i], &arr[j]) {
                return false;
            }
        }
    }
    true
}

fn number_as_i128(n: &Number) -> Option<i128> {
    if let Some(i) = n.as_i64() {
        return Some(i as i128);
    }
    if let Some(u) = n.as_u64() {
        return Some(u as i128);
    }
    let f = n.as_f64()?;
    if f.is_finite() && f.fract() == 0.0 && f >= (i128::MIN as f64) && f < (i128::MAX as f64) {
        Some(f as i128)
    } else {
        None
    }
}

fn cmp_numbers(a: &Number, b: &Number) -> Ordering {
    if let (Some(ai), Some(bi)) = (number_as_i128(a), number_as_i128(b)) {
        return ai.cmp(&bi);
    }
    let af = a.as_f64().unwrap_or(0.0);
    let bf = b.as_f64().unwrap_or(0.0);
    af.total_cmp(&bf)
}

fn is_multiple_of(value: &Number, divisor: &Number) -> bool {
    if let (Some(vi), Some(di)) = (number_as_i128(value), number_as_i128(divisor)) {
        return di != 0 && vi % di == 0;
    }
    if let (Some((vc, ve)), Some((dc, de))) = (parse_decimal(value), parse_decimal(divisor)) {
        if dc == 0 {
            return false;
        }
        if vc == 0 {
            return true;
        }
        if ve >= de {
            if let Some(scale) = pow10_u128((ve - de) as u32)
                && let Some(scaled) = vc.checked_mul(scale)
            {
                return scaled % dc == 0;
            }
        } else if let Some(scale) = pow10_u128((de - ve) as u32)
            && let Some(scaled) = dc.checked_mul(scale)
        {
            return vc % scaled == 0;
        }
    }
    let vf = value.as_f64().unwrap_or(f64::NAN);
    let df = divisor.as_f64().unwrap_or(0.0);
    if !vf.is_finite() || !df.is_finite() || df == 0.0 {
        return false;
    }
    let q = vf / df;
    q.is_finite() && (q - q.round()).abs() <= 1e-12
}

fn parse_decimal(n: &Number) -> Option<(u128, i32)> {
    let s = n.to_string();
    let s = s.strip_prefix('-').unwrap_or(&s);
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(idx) => (&s[..idx], s[idx + 1..].parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (digits, frac_len) = match mantissa.find('.') {
        Some(dot) => {
            let mut buf = String::with_capacity(mantissa.len() - 1);
            buf.push_str(&mantissa[..dot]);
            buf.push_str(&mantissa[dot + 1..]);
            (buf, (mantissa.len() - dot - 1) as i32)
        }
        None => (mantissa.to_string(), 0),
    };
    let coeff = digits.parse::<u128>().ok()?;
    Some((coeff, exp - frac_len))
}

fn pow10_u128(exp: u32) -> Option<u128> {
    10_u128.checked_pow(exp)
}

fn as_usize(v: &Value) -> Option<usize> {
    let n = v.as_number()?;
    if let Some(u) = n.as_u64() {
        return usize::try_from(u).ok();
    }
    if let Some(i) = n.as_i64() {
        return usize::try_from(i).ok();
    }
    let f = n.as_f64()?;
    if f.is_finite() && f >= 0.0 && f.fract() == 0.0 {
        Some(f as usize)
    } else {
        None
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if is_json_integer(n) {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn format_expected_types(ty: &Value) -> String {
    match ty {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "valid value".into(),
    }
}

fn format_value_list(values: &[Value]) -> String {
    if values.len() <= MAX_LISTED_ENUM_VALUES {
        let items: Vec<String> = values.iter().map(Value::to_string).collect();
        return format!("[{}]", items.join(", "));
    }
    let items: Vec<String> = values[..MAX_LISTED_ENUM_VALUES]
        .iter()
        .map(Value::to_string)
        .collect();
    format!(
        "[{}, ... and {} more]",
        items.join(", "),
        values.len() - MAX_LISTED_ENUM_VALUES
    )
}

fn join_field(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_string()
    } else {
        format!("{base}.{key}")
    }
}

fn join_index(base: &str, idx: usize) -> String {
    format!("{base}[{idx}]")
}
