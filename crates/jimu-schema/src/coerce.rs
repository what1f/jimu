//! 3-pass copy-on-write loose coercion ported from `pi-schema/src/coerce.ts`.
//!
//! 1. **Pass 1 (Drop optional nulls)**: In objects, omit non-required properties whose value is
//!    `null` when the property's subschema rejects `null`, preserving key insertion order.
//! 2. **Pass 2 (Structure-aware conversion)**: Follows `$ref`, `const`, `enum`, `anyOf`/`oneOf`,
//!    `allOf`, `type`, `items`/`prefixItems`, `properties`/`additionalProperties`, coercing
//!    numeric/boolean strings, single values into one-item arrays (when the wrapped item passes
//!    the item schema), and union branches in order.
//! 3. **Pass 3 (Primitive coercion by type arrays)**: Handles multi-type schemas such as
//!    `["integer", "null"]` left untouched by Pass 2.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Number, Value};

use crate::resolve::resolve_pointer;
use crate::validate::{is_json_integer, item_schema_at, json_equal, matches_type, passes_node};

/// Hard recursion guard across Pass 1, Pass 2, and Pass 3 traversal.
const MAX_COERCE_DEPTH: usize = 256;

pub(crate) fn coerce_root(root: &Value, value: &Value) -> Value {
    let mut coercer = Coercer::new(root);
    let empty_req = HashSet::new();
    let pass1 = coercer.drop_optional_nulls(value, root, 0, 0, &empty_req);
    let pass2 = coercer.convert(root, pass1.as_ref(), 0);
    let pass3 = coercer.coerce_types(pass2.as_ref(), root, 0, 0);
    pass3.into_owned()
}

struct Coercer<'a> {
    root: &'a Value,
    /// Cycle guard for Pass 2 re-entry points (`$ref` targets and array item wrapping).
    entered: Vec<(usize, Value)>,
    /// Number of re-entry cycle cuts encountered so far; results across cuts are not memoized.
    cuts: usize,
    /// Memoized `$ref` conversion results keyed by `(schema_ptr, exact_json_representation)`.
    /// Using the exact JSON string preserves object key order across elements.
    /// `None` means conversion left the input unchanged (`Cow::Borrowed`).
    memo: HashMap<(usize, String), Option<Value>>,
    /// Cycle guard for Pass 1 `$ref` traversal: `(target_schema_ptr, value_depth)`.
    pass1_ref_guard: Vec<(usize, usize)>,
    /// Cycle guard for Pass 3 `$ref` traversal: `(target_schema_ptr, value_depth)`.
    pass3_ref_guard: Vec<(usize, usize)>,
}

impl<'a> Coercer<'a> {
    fn new(root: &'a Value) -> Self {
        Self {
            root,
            entered: Vec::new(),
            cuts: 0,
            memo: HashMap::new(),
            pass1_ref_guard: Vec::new(),
            pass3_ref_guard: Vec::new(),
        }
    }

    fn passes(&self, node: &Value, value: &Value) -> bool {
        passes_node(self.root, node, value)
    }

    fn ptr(node: &Value) -> usize {
        node as *const Value as usize
    }

    fn collect_required_keys(
        &self,
        schema: &'a Value,
        out: &mut HashSet<&'a str>,
        visited: &mut Vec<usize>,
    ) {
        let Some(map) = schema.as_object() else {
            return;
        };
        let node_ptr = Self::ptr(schema);
        if visited.contains(&node_ptr) {
            return;
        }
        visited.push(node_ptr);

        if let Some(arr) = map.get("required").and_then(Value::as_array) {
            for item in arr {
                if let Some(key) = item.as_str() {
                    out.insert(key);
                }
            }
        }
        if let Some(ref_str) = map.get("$ref").and_then(Value::as_str)
            && let Ok(target) = resolve_pointer(self.root, ref_str)
        {
            self.collect_required_keys(target, out, visited);
        }
        if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
            for member in all_of {
                self.collect_required_keys(member, out, visited);
            }
        }
        visited.pop();
    }

    // -------------------------------------------------------------------------
    // Pass 1: Drop optional nulls
    // -------------------------------------------------------------------------

    fn drop_optional_nulls<'v>(
        &mut self,
        value: &'v Value,
        schema: &'a Value,
        value_depth: usize,
        depth: usize,
        inherited_required: &HashSet<&'a str>,
    ) -> Cow<'v, Value> {
        if depth >= MAX_COERCE_DEPTH {
            return Cow::Borrowed(value);
        }
        let Some(map) = schema.as_object() else {
            return Cow::Borrowed(value);
        };

        let mut merged_required = inherited_required.clone();
        let mut req_visited = Vec::new();
        self.collect_required_keys(schema, &mut merged_required, &mut req_visited);

        if let Some(ref_str) = map.get("$ref").and_then(Value::as_str)
            && let Ok(target) = resolve_pointer(self.root, ref_str)
        {
            let guard = (Self::ptr(target), value_depth);
            if !self.pass1_ref_guard.contains(&guard) {
                self.pass1_ref_guard.push(guard);
                let out = self.drop_optional_nulls(
                    value,
                    target,
                    value_depth,
                    depth + 1,
                    &merged_required,
                );
                self.pass1_ref_guard.pop();
                return match out {
                    Cow::Borrowed(_) => self.drop_optional_nulls_node(
                        value,
                        map,
                        value_depth,
                        depth + 1,
                        &merged_required,
                    ),
                    Cow::Owned(v) => Cow::Owned(
                        self.drop_optional_nulls_node(
                            &v,
                            map,
                            value_depth,
                            depth + 1,
                            &merged_required,
                        )
                        .into_owned(),
                    ),
                };
            }
        }

        self.drop_optional_nulls_node(value, map, value_depth, depth + 1, &merged_required)
    }

    fn drop_optional_nulls_node<'v>(
        &mut self,
        value: &'v Value,
        map: &'a Map<String, Value>,
        value_depth: usize,
        depth: usize,
        required: &HashSet<&'a str>,
    ) -> Cow<'v, Value> {
        if depth >= MAX_COERCE_DEPTH {
            return Cow::Borrowed(value);
        }
        let mut current = Cow::Borrowed(value);

        if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
            for member in all_of {
                current = match current {
                    Cow::Borrowed(v) => {
                        self.drop_optional_nulls(v, member, value_depth, depth + 1, required)
                    }
                    Cow::Owned(v) => Cow::Owned(
                        self.drop_optional_nulls(&v, member, value_depth, depth + 1, required)
                            .into_owned(),
                    ),
                };
            }
        }

        if let Some(branches) = map
            .get("anyOf")
            .or_else(|| map.get("oneOf"))
            .and_then(Value::as_array)
            && !branches.iter().any(|b| self.passes(b, current.as_ref()))
        {
            for branch in branches {
                let candidate = self.drop_optional_nulls(
                    current.as_ref(),
                    branch,
                    value_depth,
                    depth + 1,
                    required,
                );
                if matches!(candidate, Cow::Owned(_)) && self.passes(branch, candidate.as_ref()) {
                    current = Cow::Owned(candidate.into_owned());
                    break;
                }
            }
        }

        match current.as_ref() {
            Value::Array(_) => match current {
                Cow::Borrowed(Value::Array(arr)) => {
                    self.drop_nulls_array(arr, map, value, value_depth, depth)
                }
                Cow::Owned(Value::Array(arr)) => {
                    let orig = Value::Array(arr);
                    let Value::Array(ref slice) = orig else {
                        unreachable!()
                    };
                    Cow::Owned(
                        self.drop_nulls_array(slice, map, &orig, value_depth, depth)
                            .into_owned(),
                    )
                }
                _ => current,
            },
            Value::Object(_) => match current {
                Cow::Borrowed(Value::Object(obj)) => {
                    self.drop_nulls_object(obj, map, value, value_depth, depth, required)
                }
                Cow::Owned(Value::Object(obj)) => {
                    let orig = Value::Object(obj);
                    let Value::Object(ref obj_ref) = orig else {
                        unreachable!()
                    };
                    Cow::Owned(
                        self.drop_nulls_object(obj_ref, map, &orig, value_depth, depth, required)
                            .into_owned(),
                    )
                }
                _ => current,
            },
            _ => current,
        }
    }

    fn drop_nulls_array<'v>(
        &mut self,
        arr: &'v [Value],
        schema: &'a Map<String, Value>,
        orig: &'v Value,
        value_depth: usize,
        depth: usize,
    ) -> Cow<'v, Value> {
        let empty_req = HashSet::new();
        let mut out: Option<Vec<Value>> = None;
        for (i, item) in arr.iter().enumerate() {
            let Some(item_schema) = item_schema_at(schema, i) else {
                continue;
            };
            let next =
                self.drop_optional_nulls(item, item_schema, value_depth + 1, depth + 1, &empty_req);
            if matches!(next, Cow::Owned(_)) {
                let vec = out.get_or_insert_with(|| arr.to_vec());
                vec[i] = next.into_owned();
            }
        }
        match out {
            Some(vec) => Cow::Owned(Value::Array(vec)),
            None => Cow::Borrowed(orig),
        }
    }

    fn drop_nulls_object<'v>(
        &mut self,
        obj: &'v Map<String, Value>,
        schema: &'a Map<String, Value>,
        orig: &'v Value,
        value_depth: usize,
        depth: usize,
        required: &HashSet<&'a str>,
    ) -> Cow<'v, Value> {
        let properties = schema.get("properties").and_then(Value::as_object);
        let additional = schema.get("additionalProperties").filter(|v| v.is_object());
        if properties.is_none() && additional.is_none() {
            return Cow::Borrowed(orig);
        }

        let empty_req = HashSet::new();

        // Check first if anything changes (copy-on-write, preserving exact key order).
        let mut changed = false;
        let mut rebuilt: Vec<(String, Value)> = Vec::new();

        for (idx, (key, val)) in obj.iter().enumerate() {
            if let Some(prop_schema) = properties.and_then(|p| p.get(key)) {
                if val.is_null()
                    && !required.contains(key.as_str())
                    && !self.passes(prop_schema, &Value::Null)
                {
                    if !changed {
                        changed = true;
                        rebuilt.reserve(obj.len());
                        for (k, v) in obj.iter().take(idx) {
                            rebuilt.push((k.clone(), v.clone()));
                        }
                    }
                    continue;
                }
                let next = self.drop_optional_nulls(
                    val,
                    prop_schema,
                    value_depth + 1,
                    depth + 1,
                    &empty_req,
                );
                if matches!(next, Cow::Owned(_)) {
                    if !changed {
                        changed = true;
                        rebuilt.reserve(obj.len());
                        for (k, v) in obj.iter().take(idx) {
                            rebuilt.push((k.clone(), v.clone()));
                        }
                    }
                    rebuilt.push((key.clone(), next.into_owned()));
                } else if changed {
                    rebuilt.push((key.clone(), val.clone()));
                }
            } else if let Some(addl_schema) = additional {
                let next = self.drop_optional_nulls(
                    val,
                    addl_schema,
                    value_depth + 1,
                    depth + 1,
                    &empty_req,
                );
                if matches!(next, Cow::Owned(_)) {
                    if !changed {
                        changed = true;
                        rebuilt.reserve(obj.len());
                        for (k, v) in obj.iter().take(idx) {
                            rebuilt.push((k.clone(), v.clone()));
                        }
                    }
                    rebuilt.push((key.clone(), next.into_owned()));
                } else if changed {
                    rebuilt.push((key.clone(), val.clone()));
                }
            } else if changed {
                rebuilt.push((key.clone(), val.clone()));
            }
        }

        if !changed {
            return Cow::Borrowed(orig);
        }
        let mut map = Map::with_capacity(rebuilt.len());
        for (k, v) in rebuilt {
            map.insert(k, v);
        }
        Cow::Owned(Value::Object(map))
    }

    // -------------------------------------------------------------------------
    // Pass 2: Structure-aware conversion
    // -------------------------------------------------------------------------

    fn reenter<'v>(
        &mut self,
        target: &'a Value,
        value: &'v Value,
        memoize: bool,
        depth: usize,
    ) -> Cow<'v, Value> {
        if depth >= MAX_COERCE_DEPTH {
            return Cow::Borrowed(value);
        }
        let target_ptr = Self::ptr(target);
        if self
            .entered
            .iter()
            .any(|(ptr, v)| *ptr == target_ptr && v == value)
        {
            self.cuts += 1;
            return Cow::Borrowed(value);
        }
        let memo_key = if memoize {
            serde_json::to_string(value).ok().map(|s| (target_ptr, s))
        } else {
            None
        };
        if let Some(ref key) = memo_key
            && let Some(cached) = self.memo.get(key)
        {
            return match cached {
                Some(v) => Cow::Owned(v.clone()),
                None => Cow::Borrowed(value),
            };
        }

        let prev_cuts = self.cuts;
        self.entered.push((target_ptr, value.clone()));
        let result = self.convert(target, value, depth + 1);
        self.entered.pop();

        if let Some(key) = memo_key
            && self.cuts == prev_cuts
        {
            let entry = match &result {
                Cow::Borrowed(_) => None,
                Cow::Owned(v) => Some(v.clone()),
            };
            self.memo.insert(key, entry);
        }
        result
    }

    fn convert<'v>(&mut self, node: &'a Value, value: &'v Value, depth: usize) -> Cow<'v, Value> {
        if depth >= MAX_COERCE_DEPTH {
            return Cow::Borrowed(value);
        }
        let Some(map) = node.as_object() else {
            return Cow::Borrowed(value);
        };
        if let Some(ref_str) = map.get("$ref").and_then(Value::as_str) {
            let Ok(target) = resolve_pointer(self.root, ref_str) else {
                return Cow::Borrowed(value);
            };
            let after_ref = if target.is_object() {
                self.reenter(target, value, true, depth + 1)
            } else {
                Cow::Borrowed(value)
            };
            if map.len() == 1 {
                return after_ref;
            }
            return match after_ref {
                Cow::Borrowed(v) => self.convert_node(node, map, v, depth + 1),
                Cow::Owned(v) => {
                    Cow::Owned(self.convert_node(node, map, &v, depth + 1).into_owned())
                }
            };
        }
        self.convert_node(node, map, value, depth + 1)
    }

    fn convert_node<'v>(
        &mut self,
        node: &'a Value,
        map: &'a Map<String, Value>,
        value: &'v Value,
        depth: usize,
    ) -> Cow<'v, Value> {
        if depth >= MAX_COERCE_DEPTH {
            return Cow::Borrowed(value);
        }
        if let Some(const_val) = map.get("const") {
            return convert_literal(const_val, value);
        }
        if let Some(enum_vals) = map.get("enum").and_then(Value::as_array) {
            return self.convert_enum(node, enum_vals, value);
        }
        if let Some(branches) = map.get("anyOf").and_then(Value::as_array) {
            return self.convert_union(node, map, branches, value, depth);
        }
        if let Some(branches) = map.get("oneOf").and_then(Value::as_array) {
            return self.convert_union(node, map, branches, value, depth);
        }
        if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
            let mut next = Cow::Borrowed(value);
            for member in all_of {
                next = match next {
                    Cow::Borrowed(v) => self.convert(member, v, depth + 1),
                    Cow::Owned(v) => Cow::Owned(self.convert(member, &v, depth + 1).into_owned()),
                };
            }
            return match next {
                Cow::Borrowed(v) => self.convert_members(map, v, depth),
                Cow::Owned(v) => Cow::Owned(self.convert_members(map, &v, depth).into_owned()),
            };
        }

        match map.get("type").and_then(Value::as_str) {
            Some("string") => try_string(value).map_or(Cow::Borrowed(value), Cow::Owned),
            Some("number") => try_number(value).map_or(Cow::Borrowed(value), Cow::Owned),
            Some("integer") => try_integer(value).map_or(Cow::Borrowed(value), Cow::Owned),
            Some("boolean") => try_boolean(value).map_or(Cow::Borrowed(value), Cow::Owned),
            Some("null") => try_null(value).map_or(Cow::Borrowed(value), Cow::Owned),
            Some("array") => self.convert_array(map, value, depth),
            Some("object") => self.convert_object(map, value, depth),
            None if map.get("type").is_none() => {
                if map.contains_key("properties") || map.contains_key("additionalProperties") {
                    self.convert_object(map, value, depth)
                } else if map.contains_key("items") || map.contains_key("prefixItems") {
                    self.convert_array(map, value, depth)
                } else {
                    Cow::Borrowed(value)
                }
            }
            _ => Cow::Borrowed(value),
        }
    }

    fn convert_enum<'v>(
        &self,
        node: &'a Value,
        values: &[Value],
        value: &'v Value,
    ) -> Cow<'v, Value> {
        if values.iter().any(|m| json_equal(m, value)) {
            return Cow::Borrowed(value);
        }
        for member in values {
            let candidate = convert_literal(member, value);
            if matches!(candidate, Cow::Owned(_)) && self.passes(node, candidate.as_ref()) {
                return candidate;
            }
        }
        Cow::Borrowed(value)
    }

    fn convert_union<'v>(
        &mut self,
        node: &'a Value,
        map: &'a Map<String, Value>,
        branches: &'a [Value],
        value: &'v Value,
        depth: usize,
    ) -> Cow<'v, Value> {
        if self.passes(node, value) || branches.iter().any(|b| self.passes(b, value)) {
            return self.convert_members(map, value, depth);
        }
        let empty_req = HashSet::new();
        for branch in branches {
            // Run full per-branch pipeline (optional-null drop + pass 2 + pass 3) so a branch
            // needing both null dropping and primitive coercion resolves cleanly.
            let p1 = self.drop_optional_nulls(value, branch, 0, depth + 1, &empty_req);
            let p2 = self.convert(branch, p1.as_ref(), depth + 1);
            let p3 = self.coerce_types(p2.as_ref(), branch, 0, depth + 1);
            let candidate = self.convert_members(map, p3.as_ref(), depth);
            if self.passes(node, candidate.as_ref()) {
                return Cow::Owned(candidate.into_owned());
            }
        }
        self.convert_members(map, value, depth)
    }

    fn convert_members<'v>(
        &mut self,
        map: &'a Map<String, Value>,
        value: &'v Value,
        depth: usize,
    ) -> Cow<'v, Value> {
        if map.contains_key("properties") || map.contains_key("additionalProperties") {
            self.convert_object(map, value, depth)
        } else {
            Cow::Borrowed(value)
        }
    }

    fn convert_array<'v>(
        &mut self,
        map: &'a Map<String, Value>,
        value: &'v Value,
        depth: usize,
    ) -> Cow<'v, Value> {
        let prefix = map
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| map.get("prefixItems").and_then(Value::as_array));

        if let Some(prefix_arr) = prefix {
            // Tuple schema: coerce elements in place, never wrap a single value.
            let Value::Array(items) = value else {
                return Cow::Borrowed(value);
            };
            let rest = if map.get("items").is_some_and(Value::is_array) {
                None
            } else {
                map.get("items")
            };
            let mut out: Option<Vec<Value>> = None;
            for (i, item) in items.iter().enumerate() {
                let item_schema = if i < prefix_arr.len() {
                    Some(&prefix_arr[i])
                } else {
                    rest
                };
                if let Some(s) = item_schema {
                    let next = self.convert(s, item, depth + 1);
                    if matches!(next, Cow::Owned(_)) {
                        let vec = out.get_or_insert_with(|| items.clone());
                        vec[i] = next.into_owned();
                    }
                }
            }
            return match out {
                Some(vec) => Cow::Owned(Value::Array(vec)),
                None => Cow::Borrowed(value),
            };
        }

        if let Value::Array(items) = value {
            let Some(item_schema) = map.get("items") else {
                return Cow::Borrowed(value);
            };
            let mut out: Option<Vec<Value>> = None;
            for (i, item) in items.iter().enumerate() {
                let next = self.convert(item_schema, item, depth + 1);
                if matches!(next, Cow::Owned(_)) {
                    let vec = out.get_or_insert_with(|| items.clone());
                    vec[i] = next.into_owned();
                }
            }
            return match out {
                Some(vec) => Cow::Owned(Value::Array(vec)),
                None => Cow::Borrowed(value),
            };
        }

        // Single non-array value -> wrap into one-item array ONLY when the wrapped (and coerced)
        // item passes the item schema. Uses `reenter` to guard recursive array schemas.
        let empty_req = HashSet::new();
        match map.get("items") {
            Some(item_schema) => {
                let wrapped_item = if item_schema.is_object() {
                    let p1 = self.drop_optional_nulls(value, item_schema, 0, depth + 1, &empty_req);
                    let p2 = self.reenter(item_schema, p1.as_ref(), false, depth + 1);
                    self.coerce_types(p2.as_ref(), item_schema, 0, depth + 1)
                        .into_owned()
                } else {
                    value.clone()
                };
                if self.passes(item_schema, &wrapped_item) {
                    Cow::Owned(Value::Array(vec![wrapped_item]))
                } else {
                    Cow::Borrowed(value)
                }
            }
            None => {
                if value.is_null() {
                    Cow::Borrowed(value)
                } else {
                    Cow::Owned(Value::Array(vec![value.clone()]))
                }
            }
        }
    }

    fn convert_object<'v>(
        &mut self,
        map: &'a Map<String, Value>,
        value: &'v Value,
        depth: usize,
    ) -> Cow<'v, Value> {
        let Value::Object(obj) = value else {
            return Cow::Borrowed(value);
        };
        let properties = map.get("properties").and_then(Value::as_object);
        let additional = map.get("additionalProperties").filter(|v| v.is_object());
        if properties.is_none() && additional.is_none() {
            return Cow::Borrowed(value);
        }

        let mut out: Option<Map<String, Value>> = None;
        for (key, val) in obj {
            let next = if let Some(prop_schema) = properties.and_then(|p| p.get(key)) {
                self.convert(prop_schema, val, depth + 1)
            } else if let Some(addl) = additional {
                self.convert(addl, val, depth + 1)
            } else {
                Cow::Borrowed(val)
            };
            if matches!(next, Cow::Owned(_)) {
                let target = out.get_or_insert_with(|| obj.clone());
                target.insert(key.clone(), next.into_owned());
            }
        }

        match out {
            Some(m) => Cow::Owned(Value::Object(m)),
            None => Cow::Borrowed(value),
        }
    }

    // -------------------------------------------------------------------------
    // Pass 3: Primitive coercion by type (handles type arrays like ["integer", "null"])
    // -------------------------------------------------------------------------

    fn coerce_types<'v>(
        &mut self,
        value: &'v Value,
        schema: &'a Value,
        value_depth: usize,
        depth: usize,
    ) -> Cow<'v, Value> {
        if depth >= MAX_COERCE_DEPTH {
            return Cow::Borrowed(value);
        }
        let Some(map) = schema.as_object() else {
            return Cow::Borrowed(value);
        };

        let mut current = Cow::Borrowed(value);

        if let Some(ref_str) = map.get("$ref").and_then(Value::as_str)
            && let Ok(target) = resolve_pointer(self.root, ref_str)
        {
            let guard = (Self::ptr(target), value_depth);
            if !self.pass3_ref_guard.contains(&guard) {
                self.pass3_ref_guard.push(guard);
                current = match current {
                    Cow::Borrowed(v) => self.coerce_types(v, target, value_depth, depth + 1),
                    Cow::Owned(v) => Cow::Owned(
                        self.coerce_types(&v, target, value_depth, depth + 1)
                            .into_owned(),
                    ),
                };
                self.pass3_ref_guard.pop();
            }
        }

        if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
            for member in all_of {
                current = match current {
                    Cow::Borrowed(v) => self.coerce_types(v, member, value_depth, depth + 1),
                    Cow::Owned(v) => Cow::Owned(
                        self.coerce_types(&v, member, value_depth, depth + 1)
                            .into_owned(),
                    ),
                };
            }
        }

        if let Some(any_of) = map.get("anyOf").and_then(Value::as_array) {
            current = match current {
                Cow::Borrowed(v) => self.coerce_union(v, any_of, value_depth, depth + 1),
                Cow::Owned(v) => Cow::Owned(
                    self.coerce_union(&v, any_of, value_depth, depth + 1)
                        .into_owned(),
                ),
            };
        }

        if let Some(one_of) = map.get("oneOf").and_then(Value::as_array) {
            current = match current {
                Cow::Borrowed(v) => self.coerce_union(v, one_of, value_depth, depth + 1),
                Cow::Owned(v) => Cow::Owned(
                    self.coerce_union(&v, one_of, value_depth, depth + 1)
                        .into_owned(),
                ),
            };
        }

        let types = types_of(map);
        let already_matches =
            types.len() > 1 && types.iter().any(|t| matches_type(current.as_ref(), t));
        if !types.is_empty() && !already_matches {
            for ty in &types {
                if let Some(coerced) = coerce_primitive(current.as_ref(), ty) {
                    current = Cow::Owned(coerced);
                    break;
                }
            }
        }

        if (types.contains(&"object") || types.is_empty()) && current.is_object() {
            current = match current {
                Cow::Borrowed(v) => self.coerce_properties(v, map, value_depth, depth),
                Cow::Owned(v) => Cow::Owned(
                    self.coerce_properties(&v, map, value_depth, depth)
                        .into_owned(),
                ),
            };
        }

        if (types.contains(&"array") || types.is_empty()) && current.is_array() {
            current = match current {
                Cow::Borrowed(v) => self.coerce_items(v, map, value_depth, depth),
                Cow::Owned(v) => {
                    Cow::Owned(self.coerce_items(&v, map, value_depth, depth).into_owned())
                }
            };
        }

        current
    }

    fn coerce_union<'v>(
        &mut self,
        value: &'v Value,
        branches: &'a [Value],
        value_depth: usize,
        depth: usize,
    ) -> Cow<'v, Value> {
        if branches.iter().any(|b| self.passes(b, value)) {
            return Cow::Borrowed(value);
        }
        for branch in branches {
            let candidate = self.coerce_types(value, branch, value_depth, depth + 1);
            if self.passes(branch, candidate.as_ref()) {
                return candidate;
            }
        }
        Cow::Borrowed(value)
    }

    fn coerce_properties<'v>(
        &mut self,
        value: &'v Value,
        schema: &'a Map<String, Value>,
        value_depth: usize,
        depth: usize,
    ) -> Cow<'v, Value> {
        let Value::Object(obj) = value else {
            return Cow::Borrowed(value);
        };
        let properties = schema.get("properties").and_then(Value::as_object);
        let additional = schema.get("additionalProperties").filter(|v| v.is_object());
        if properties.is_none() && additional.is_none() {
            return Cow::Borrowed(value);
        }

        let mut out: Option<Map<String, Value>> = None;
        for (key, val) in obj {
            let next = if let Some(prop_schema) = properties.and_then(|p| p.get(key)) {
                self.coerce_types(val, prop_schema, value_depth + 1, depth + 1)
            } else if let Some(addl) = additional {
                self.coerce_types(val, addl, value_depth + 1, depth + 1)
            } else {
                Cow::Borrowed(val)
            };
            if matches!(next, Cow::Owned(_)) {
                let target = out.get_or_insert_with(|| obj.clone());
                target.insert(key.clone(), next.into_owned());
            }
        }

        match out {
            Some(m) => Cow::Owned(Value::Object(m)),
            None => Cow::Borrowed(value),
        }
    }

    fn coerce_items<'v>(
        &mut self,
        value: &'v Value,
        schema: &'a Map<String, Value>,
        value_depth: usize,
        depth: usize,
    ) -> Cow<'v, Value> {
        let Value::Array(items) = value else {
            return Cow::Borrowed(value);
        };
        let mut out: Option<Vec<Value>> = None;
        for (i, item) in items.iter().enumerate() {
            let Some(item_schema) = item_schema_at(schema, i) else {
                continue;
            };
            let next = self.coerce_types(item, item_schema, value_depth + 1, depth + 1);
            if matches!(next, Cow::Owned(_)) {
                let vec = out.get_or_insert_with(|| items.clone());
                vec[i] = next.into_owned();
            }
        }
        match out {
            Some(vec) => Cow::Owned(Value::Array(vec)),
            None => Cow::Borrowed(value),
        }
    }
}

// -----------------------------------------------------------------------------
// Primitive conversion helpers
// -----------------------------------------------------------------------------

fn convert_literal<'v>(literal: &Value, value: &'v Value) -> Cow<'v, Value> {
    if json_equal(literal, value) {
        return Cow::Borrowed(value);
    }
    let converted = match literal {
        Value::String(_) => try_string(value),
        Value::Number(n) if is_json_integer(n) => try_integer(value).or_else(|| try_number(value)),
        Value::Number(_) => try_number(value),
        Value::Bool(_) => try_boolean(value),
        Value::Null => try_null(value),
        _ => None,
    };
    match converted {
        Some(v) if json_equal(literal, &v) => Cow::Owned(v),
        _ => Cow::Borrowed(value),
    }
}

fn try_string(value: &Value) -> Option<Value> {
    match value {
        Value::String(_) => None,
        Value::Bool(b) => Some(Value::String(b.to_string())),
        Value::Number(n) => Some(Value::String(n.to_string())),
        _ => None,
    }
}

fn try_number(value: &Value) -> Option<Value> {
    match value {
        Value::Number(_) => None,
        Value::Bool(b) => Some(Value::Number(Number::from(if *b { 1 } else { 0 }))),
        Value::String(s) => parse_number_str(s).map(Value::Number),
        _ => None,
    }
}

fn try_integer(value: &Value) -> Option<Value> {
    match value {
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                return None;
            }
            let f = n.as_f64()?;
            if !f.is_finite() {
                return None;
            }
            trunc_f64_to_number(f).map(Value::Number)
        }
        Value::Bool(b) => Some(Value::Number(Number::from(if *b { 1 } else { 0 }))),
        Value::String(s) => parse_integer_str(s).map(Value::Number),
        _ => None,
    }
}

fn try_boolean(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(_) => None,
        Value::Number(n) => {
            if n.as_i64() == Some(0) || n.as_u64() == Some(0) || n.as_f64() == Some(0.0) {
                Some(Value::Bool(false))
            } else if n.as_i64() == Some(1) || n.as_u64() == Some(1) || n.as_f64() == Some(1.0) {
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("true") || trimmed == "1" {
                Some(Value::Bool(true))
            } else if trimmed.eq_ignore_ascii_case("false") || trimmed == "0" {
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn try_null(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Bool(false) => Some(Value::Null),
        Value::Number(n)
            if n.as_i64() == Some(0) || n.as_u64() == Some(0) || n.as_f64() == Some(0.0) =>
        {
            Some(Value::Null)
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty()
                || trimmed == "0"
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("undefined")
            {
                Some(Value::Null)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_number_str(s: &str) -> Option<Number> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        return Some(Number::from(i));
    }
    if let Ok(u) = trimmed.parse::<u64>() {
        return Some(Number::from(u));
    }
    if let Ok(f) = trimmed.parse::<f64>()
        && f.is_finite()
    {
        return Number::from_f64(f);
    }
    None
}

fn parse_integer_str(s: &str) -> Option<Number> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        return Some(Number::from(i));
    }
    if let Ok(u) = trimmed.parse::<u64>() {
        return Some(Number::from(u));
    }
    if let Ok(f) = trimmed.parse::<f64>()
        && f.is_finite()
    {
        return trunc_f64_to_number(f);
    }
    None
}

fn trunc_f64_to_number(f: f64) -> Option<Number> {
    let t = f.trunc();
    if !t.is_finite() {
        return None;
    }
    if t >= (i64::MIN as f64) && t < (i64::MAX as f64) {
        Some(Number::from(t as i64))
    } else if t >= 0.0 && t < (u64::MAX as f64) {
        Some(Number::from(t as u64))
    } else {
        Number::from_f64(t)
    }
}

fn types_of(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(arr)) => arr.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn coerce_primitive(value: &Value, ty: &str) -> Option<Value> {
    match ty {
        "number" => coerce_primitive_number(value),
        "integer" => coerce_primitive_integer(value),
        "boolean" => coerce_primitive_boolean(value),
        "string" => try_string(value),
        "null" => coerce_primitive_null(value),
        _ => None,
    }
}

fn coerce_primitive_number(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => parse_number_str(s).map(Value::Number),
        _ => None,
    }
}

fn coerce_primitive_integer(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(i) = trimmed.parse::<i64>() {
                return Some(Value::Number(Number::from(i)));
            }
            if let Ok(u) = trimmed.parse::<u64>() {
                return Some(Value::Number(Number::from(u)));
            }
            if let Ok(f) = trimmed.parse::<f64>()
                && f.is_finite()
                && f.fract() == 0.0
            {
                return trunc_f64_to_number(f).map(Value::Number);
            }
            None
        }
        _ => None,
    }
}

fn coerce_primitive_boolean(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("true") {
                Some(Value::Bool(true))
            } else if trimmed.eq_ignore_ascii_case("false") {
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn coerce_primitive_null(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) if s.trim().eq_ignore_ascii_case("null") => Some(Value::Null),
        _ => None,
    }
}
