use jimu_schema::{
    CompiledSchema, SchemaCompileError, all_of, any_of, arr_schema, bool_schema, const_schema,
    int_schema, num_schema, obj_schema, one_of, opt_nullable, ref_schema, str_schema, string_enum,
    tuple_schema, with_additional_properties, with_bounds, with_default, with_defs,
    with_description, with_format, with_items_bounds, with_length, with_unique_items,
};
use serde_json::{Value, json};

#[test]
fn llm_tool_call_coercion_and_key_order_preservation() {
    let filter_schema = obj_schema([
        (
            "pattern",
            with_description(str_schema(), "Glob pattern"),
            true,
        ),
        (
            "max_depth",
            with_bounds(int_schema(), Some(1.0), Some(10.0)),
            false,
        ),
        ("include_hidden", bool_schema(), false),
    ]);

    let tool_schema = with_description(
        obj_schema([
            ("path", with_length(str_schema(), Some(1), None), true),
            ("offset", with_bounds(int_schema(), Some(1.0), None), false),
            (
                "temperature",
                with_bounds(num_schema(), Some(0.0), Some(2.0)),
                false,
            ),
            ("dry_run", with_default(bool_schema(), json!(false)), false),
            (
                "tags",
                with_unique_items(arr_schema(str_schema()), true),
                false,
            ),
            ("counts", arr_schema(int_schema()), false),
            ("cursor", opt_nullable(str_schema()), false),
            ("filter", filter_schema, false),
            ("mode", string_enum(["fast", "thorough"]), true),
        ]),
        "Search files in workspace",
    );

    let compiled = CompiledSchema::compile(tool_schema.clone()).expect("valid tool schema");
    // Prompt cache invariant: exact JSON key ordering is byte-identical after compile.
    assert_eq!(
        serde_json::to_string(compiled.schema()).unwrap(),
        serde_json::to_string(&tool_schema).unwrap()
    );

    // Realistic messy LLM tool-call arguments:
    // - numeric/float/boolean strings
    // - single values where arrays are expected
    // - nested optional nulls dropped while nullable `cursor: null` is preserved
    // - key insertion order preserved after dropping optional null
    let raw_args = json!({
        "path": "crates/jimu-schema",
        "offset": "3.5",
        "temperature": "0.75",
        "dry_run": "true",
        "tags": "rust",
        "counts": "42",
        "cursor": null,
        "filter": {
            "pattern": "*.rs",
            "max_depth": null,
            "include_hidden": "false"
        },
        "mode": "fast"
    });

    let coerced = compiled
        .coerce_and_validate(&raw_args)
        .expect("coerced tool arguments should validate");

    assert_eq!(
        coerced,
        json!({
            "path": "crates/jimu-schema",
            "offset": 3,
            "temperature": 0.75,
            "dry_run": true,
            "tags": ["rust"],
            "counts": [42],
            "cursor": null,
            "filter": {
                "pattern": "*.rs",
                "include_hidden": false
            },
            "mode": "fast"
        })
    );

    let keys: Vec<&str> = coerced
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "path",
            "offset",
            "temperature",
            "dry_run",
            "tags",
            "counts",
            "cursor",
            "filter",
            "mode"
        ]
    );

    // Uncoercible single value for integer array stays untouched so validate reports array type error.
    let bad_array_arg = json!({
        "path": "src/lib.rs",
        "counts": "not-a-number",
        "mode": "fast"
    });
    let err = compiled
        .coerce_and_validate(&bad_array_arg)
        .expect_err("uncoercible array item must fail validation");
    assert_eq!(err.issues.len(), 1);
    assert_eq!(err.issues[0].path, "counts");
    assert_eq!(err.issues[0].message, "expected array, got string");

    // Pass 3 handles multi-type arrays like ["integer", "null"].
    let nullable_type_arr = CompiledSchema::compile(json!({
        "type": "object",
        "properties": {
            "limit": { "type": ["integer", "null"], "minimum": 1 }
        }
    }))
    .unwrap();
    assert_eq!(
        nullable_type_arr
            .coerce_and_validate(&json!({ "limit": "25" }))
            .unwrap(),
        json!({ "limit": 25 })
    );
    assert_eq!(
        nullable_type_arr
            .coerce_and_validate(&json!({ "limit": null }))
            .unwrap(),
        json!({ "limit": null })
    );
}

#[test]
fn unions_combinators_tuples_and_model_friendly_diagnostics() {
    // Union coercion chooses the first matching branch cleanly.
    let int_or_bool = CompiledSchema::compile(any_of([int_schema(), bool_schema()])).unwrap();
    assert_eq!(
        int_or_bool.coerce_and_validate(&json!("-7")).unwrap(),
        json!(-7)
    );
    assert_eq!(
        int_or_bool.coerce_and_validate(&json!("true")).unwrap(),
        json!(true)
    );

    // Tagged union + nested array item diagnostics.
    let action_schema = CompiledSchema::compile(one_of([
        obj_schema([
            ("kind", const_schema(json!("batch")), true),
            (
                "items",
                with_items_bounds(
                    arr_schema(obj_schema([(
                        "limit",
                        with_bounds(int_schema(), Some(1.0), None),
                        true,
                    )])),
                    Some(1),
                    None,
                ),
                true,
            ),
        ]),
        obj_schema([
            ("kind", const_schema(json!("ping")), true),
            ("mode", string_enum(["a", "b"]), true),
        ]),
    ]))
    .unwrap();

    let err = action_schema
        .validate(&json!({
            "kind": "batch",
            "items": [
                { "limit": 5 },
                { "limit": 3.0 },
                { "limit": 0 }
            ]
        }))
        .expect_err("limit 0 violates minimum");
    assert_eq!(err.issues.len(), 1);
    assert_eq!(err.issues[0].path, "items[2].limit");
    assert_eq!(err.issues[0].message, "must be >= 1");
    assert_eq!(err.to_string(), "- items[2].limit: must be >= 1");

    // Multiple clear diagnostics on a single object.
    let read_schema = CompiledSchema::compile(obj_schema([
        ("path", str_schema(), true),
        ("limit", with_bounds(int_schema(), Some(1.0), None), false),
        ("mode", string_enum(["a", "b"]), true),
    ]))
    .unwrap();
    let multi_err = read_schema
        .validate(&json!({
            "limit": "ten",
            "mode": "c",
            "extra": true
        }))
        .expect_err("multiple issues expected");
    let rendered = multi_err.to_string();
    assert!(rendered.contains("- path: missing"), "got: {rendered}");
    assert!(
        rendered.contains("- limit: expected integer, got string"),
        "got: {rendered}"
    );
    assert!(
        rendered.contains("- mode: must be one of [\"a\", \"b\"]"),
        "got: {rendered}"
    );
    assert!(
        rendered.contains("- extra: unexpected property"),
        "got: {rendered}"
    );

    // Tuple (`prefixItems` + `items: false`), `allOf`, `not`, `additionalProperties` subschema, boolean schemas.
    let tuple = CompiledSchema::compile(tuple_schema([str_schema(), int_schema()])).unwrap();
    assert_eq!(
        tuple.coerce_and_validate(&json!(["row", "9"])).unwrap(),
        json!(["row", 9])
    );
    assert!(tuple.validate(&json!(["row", 9, "extra"])).is_err());

    let combo = CompiledSchema::compile(all_of([
        with_additional_properties(
            obj_schema([("id", str_schema(), true)]),
            int_schema(),
        ),
        json!({ "minProperties": 1, "maxProperties": 3, "not": { "properties": { "id": { "const": "forbidden" } } } }),
    ]))
    .unwrap();
    assert!(combo.validate(&json!({ "id": "ok", "score": 10 })).is_ok());
    assert!(combo.validate(&json!({ "id": "forbidden" })).is_err());
    assert!(
        combo
            .validate(&json!({ "id": "ok", "bad": "str" }))
            .is_err()
    );

    assert!(
        CompiledSchema::compile(Value::Bool(true))
            .unwrap()
            .validate(&json!(123))
            .is_ok()
    );
    assert!(
        CompiledSchema::compile(Value::Bool(false))
            .unwrap()
            .validate(&json!(123))
            .is_err()
    );
}

#[test]
fn recursive_refs_and_cycle_termination() {
    // Recursive tree using `#/$defs/Node`.
    let tree_schema = CompiledSchema::compile(with_defs(
        ref_schema("#/$defs/Node"),
        [(
            "Node",
            obj_schema([
                ("name", str_schema(), true),
                ("weight", int_schema(), false),
                ("children", arr_schema(ref_schema("#/$defs/Node")), false),
            ]),
        )],
    ))
    .expect("recursive $defs schema should compile");

    let coerced = tree_schema
        .coerce_and_validate(&json!({
            "name": "root",
            "weight": "10",
            "children": {
                "name": "leaf",
                "weight": null
            }
        }))
        .expect("recursive single-child wrap & optional null drop should succeed");
    assert_eq!(
        coerced,
        json!({
            "name": "root",
            "weight": 10,
            "children": [{ "name": "leaf" }]
        })
    );

    // Direct self-reference (`#`) with recursive array-wrapping must terminate on uncoercible input.
    let nested_strings =
        CompiledSchema::compile(any_of([str_schema(), arr_schema(ref_schema("#"))])).unwrap();
    assert!(nested_strings.validate(&json!(["a", ["b", ["c"]]])).is_ok());
    let uncoercible = json!({ "unexpected": 1 });
    assert_eq!(nested_strings.coerce(&uncoercible), uncoercible);
    assert!(nested_strings.validate(&uncoercible).is_err());

    // `#/definitions/...` support and compile-time error on dangling `$ref`.
    let draft7_ref = CompiledSchema::compile(json!({
        "definitions": { "Id": { "type": "string", "minLength": 2 } },
        "$ref": "#/definitions/Id"
    }))
    .unwrap();
    assert!(draft7_ref.validate(&json!("ab")).is_ok());
    assert!(draft7_ref.validate(&json!("a")).is_err());

    let bad_ref: Result<CompiledSchema, SchemaCompileError> =
        CompiledSchema::compile(ref_schema("#/$defs/DoesNotExist"));
    assert!(bad_ref.is_err());
}

#[test]
fn unicode_lengths_formats_and_numeric_precision() {
    // Unicode scalar count: "🚀🎉" is 2 chars (8 UTF-8 bytes), "你好世界" is 4 chars (12 bytes).
    let unicode_schema =
        CompiledSchema::compile(with_length(str_schema(), Some(2), Some(4))).unwrap();
    assert!(unicode_schema.validate(&json!("🚀🎉")).is_ok());
    assert!(unicode_schema.validate(&json!("你好世界")).is_ok());
    assert!(unicode_schema.validate(&json!("🚀")).is_err());
    assert!(unicode_schema.validate(&json!("🚀🎉✨🔥🌟")).is_err());

    // Exact decimal `multipleOf` (floating-point trap: 0.0075 / 0.0001) and float integer `3.0`.
    let num_checks = CompiledSchema::compile(json!({
        "type": "object",
        "properties": {
            "step": { "type": "number", "multipleOf": 0.0001, "exclusiveMinimum": 0, "exclusiveMaximum": 1 },
            "whole": { "type": "integer", "minimum": 1 }
        },
        "required": ["step", "whole"]
    }))
    .unwrap();
    assert!(
        num_checks
            .validate(&json!({ "step": 0.0075, "whole": 3.0 }))
            .is_ok()
    );
    assert!(
        num_checks
            .validate(&json!({ "step": 0.00755, "whole": 3 }))
            .is_err()
    );
    assert!(
        num_checks
            .validate(&json!({ "step": 0, "whole": 3 }))
            .is_err()
    );

    // Lightweight zero-regex format validators.
    let cases = [
        ("uri", "urn:isbn:0451450523", "not a uri"),
        (
            "url",
            "https://example.com:8443/v1?q=1#top",
            "urn:isbn:0451450523",
        ),
        ("email", "dev+tag@sub.example.com", "invalid@@example"),
        (
            "date-time",
            "2026-02-28T23:59:59.123+08:00",
            "2026-02-29T12:00:00Z",
        ),
        ("date", "2024-02-29", "2025-02-29"),
        (
            "uuid",
            "550e8400-e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-44665544000z",
        ),
        ("ipv4", "127.0.0.1", "256.0.0.1"),
        ("ipv6", "2001:db8::1", "2001:db8:::1"),
        ("json-pointer", "/properties/a~1b/c~0d", "no-leading-slash"),
    ];
    for (fmt, good, bad) in cases {
        let s = CompiledSchema::compile(with_format(str_schema(), fmt)).unwrap();
        assert!(
            s.validate(&json!(good)).is_ok(),
            "format {fmt} should accept {good}"
        );
        let err = s.validate(&json!(bad)).unwrap_err();
        assert_eq!(err.issues[0].message, format!("must be a valid {fmt}"));
    }
}

#[test]
fn review_regressions_formats_cycles_key_order_precision_and_pointers() {
    // 1. Multi-byte UTF-8 input >= 20 bytes where byte offset 10 splits a UTF-8 char must never panic.
    let dt_schema = CompiledSchema::compile(with_format(str_schema(), "date-time")).unwrap();
    let d_schema = CompiledSchema::compile(with_format(str_schema(), "date")).unwrap();
    for weird in ["你好世界你好世界", "🚀🚀🚀🚀🚀🚀", "2024-01-0好T00:00:00Z"]
    {
        assert!(dt_schema.validate(&json!(weird)).is_err());
        assert!(d_schema.validate(&json!(weird)).is_err());
    }

    // 2. IPv4, IPv6 bracket literal + IPvFuture in URI/URL, port <= 65535, and single-label email.
    let uri_schema = CompiledSchema::compile(with_format(str_schema(), "uri")).unwrap();
    assert!(uri_schema.validate(&json!("http://[::1]:8080/api")).is_ok());
    assert!(
        uri_schema
            .validate(&json!("http://[v1.fe80::a+en1]/x"))
            .is_ok()
    );
    assert!(
        uri_schema
            .validate(&json!("https://example.com:65535/"))
            .is_ok()
    );
    assert!(uri_schema.validate(&json!("http://[not-ipv6]/x")).is_err());
    assert!(
        uri_schema
            .validate(&json!("https://example.com:65536/"))
            .is_err()
    );
    assert!(uri_schema.validate(&json!("http://[::1]:99999/")).is_err());

    let email_schema = CompiledSchema::compile(with_format(str_schema(), "email")).unwrap();
    assert!(email_schema.validate(&json!("dev@localhost")).is_ok());
    assert!(email_schema.validate(&json!("dev@12345")).is_err());

    // 3. Pass 1 & Pass 3 `$ref` cycles combined with `allOf` producing Cow::Owned must terminate cleanly.
    let cyclic_allof = CompiledSchema::compile(json!({
        "$defs": {
            "Loop": {
                "allOf": [
                    {
                        "type": "object",
                        "properties": {
                            "n": { "type": ["integer", "null"] },
                            "opt": { "type": "string" }
                        }
                    },
                    { "$ref": "#/$defs/Loop" }
                ]
            }
        },
        "$ref": "#/$defs/Loop"
    }))
    .unwrap();
    assert_eq!(
        cyclic_allof.coerce(&json!({ "n": "42", "opt": null })),
        json!({ "n": 42 })
    );

    // 4. Pass 2 `$ref` memoization must preserve exact object key order per array element.
    let item_list = CompiledSchema::compile(json!({
        "$defs": {
            "Item": {
                "type": "object",
                "properties": {
                    "n": { "type": "integer" },
                    "x": { "type": "integer" }
                }
            }
        },
        "type": "array",
        "items": { "$ref": "#/$defs/Item" }
    }))
    .unwrap();
    let coerced_list = item_list
        .coerce_and_validate(&json!([
            { "n": "1", "x": 0 },
            { "x": 0, "n": "1" }
        ]))
        .unwrap();
    assert_eq!(
        serde_json::to_string(&coerced_list).unwrap(),
        r#"[{"n":1,"x":0},{"x":0,"n":1}]"#
    );

    // 5. Pass 3 primitive coercion rules: no truncation of "3.5" on ["integer", "null"],
    //    and ["boolean", "number"] coerces "1" -> 1 while "true"/"false" -> bool.
    let nullable_int = CompiledSchema::compile(json!({ "type": ["integer", "null"] })).unwrap();
    assert_eq!(nullable_int.coerce(&json!("3.5")), json!("3.5"));
    assert!(nullable_int.coerce_and_validate(&json!("3.5")).is_err());

    let bool_or_num = CompiledSchema::compile(json!({ "type": ["boolean", "number"] })).unwrap();
    assert_eq!(
        bool_or_num.coerce_and_validate(&json!("1")).unwrap(),
        json!(1)
    );
    assert_eq!(
        bool_or_num.coerce_and_validate(&json!("true")).unwrap(),
        json!(true)
    );
    assert_eq!(
        bool_or_num.coerce_and_validate(&json!("false")).unwrap(),
        json!(false)
    );

    // 6. Pass 1 optional `$ref` property null handling + inherited `required` across `$ref`.
    let ref_nulls = CompiledSchema::compile(json!({
        "$defs": {
            "NullableStr": { "type": ["string", "null"] },
            "StrictStr": { "type": "string" },
            "Base": {
                "type": "object",
                "properties": {
                    "req_field": { "$ref": "#/$defs/StrictStr" },
                    "opt_nullable": { "$ref": "#/$defs/NullableStr" },
                    "opt_strict": { "$ref": "#/$defs/StrictStr" }
                }
            }
        },
        "$ref": "#/$defs/Base",
        "required": ["req_field"]
    }))
    .unwrap();
    assert_eq!(
        ref_nulls.coerce(&json!({
            "req_field": null,
            "opt_nullable": null,
            "opt_strict": null
        })),
        json!({
            "req_field": null,
            "opt_nullable": null
        })
    );

    // 7. Huge bounds (u64::MAX / 1e30) must reject short strings/arrays/objects instead of being skipped.
    let huge_str =
        CompiledSchema::compile(json!({ "type": "string", "minLength": u64::MAX })).unwrap();
    assert!(huge_str.validate(&json!("abc")).is_err());
    let huge_arr = CompiledSchema::compile(json!({ "type": "array", "minItems": 1e30 })).unwrap();
    assert!(huge_arr.validate(&json!([1, 2])).is_err());
    let huge_obj =
        CompiledSchema::compile(json!({ "type": "object", "minProperties": u64::MAX })).unwrap();
    assert!(huge_obj.validate(&json!({ "a": 1 })).is_err());

    // 8. Exact `multipleOf` precision even when decimal scaling exceeds u128 range.
    let mult_one = CompiledSchema::compile(json!({ "type": "number", "multipleOf": 1 })).unwrap();
    assert!(mult_one.validate(&json!(1e-50)).is_err());
    assert!(mult_one.validate(&json!(1e50)).is_ok());
    let mult_three = CompiledSchema::compile(json!({ "type": "number", "multipleOf": 3 })).unwrap();
    assert!(mult_three.validate(&json!(1e50)).is_err());
    let mult_tiny =
        CompiledSchema::compile(json!({ "type": "number", "multipleOf": 1e-52 })).unwrap();
    assert!(mult_tiny.validate(&json!(1e-50)).is_ok());

    // 9. RFC 6901 JSON Pointer percent-decoding before splitting, ~1/~0 unescaping, and rejecting leading zeros.
    let pct_pointer = CompiledSchema::compile(json!({
        "$defs": {
            "a/b": { "type": "integer" },
            "t~k": { "type": "boolean" }
        },
        "type": "object",
        "properties": {
            "slash": { "$ref": "#%2F%24defs%2Fa~1b" },
            "tilde": { "$ref": "#/$defs/t~0k" }
        }
    }))
    .unwrap();
    assert!(
        pct_pointer
            .validate(&json!({ "slash": 7, "tilde": true }))
            .is_ok()
    );
    assert!(pct_pointer.validate(&json!({ "slash": "bad" })).is_err());

    let leading_zero_idx = CompiledSchema::compile(json!({
        "allOf": [{ "type": "string" }, { "minLength": 1 }],
        "$ref": "#/allOf/01"
    }));
    assert!(leading_zero_idx.is_err());
}

#[test]
fn string_pattern_validation_redos_safety_and_compile_time_rejection() {
    // 1. Anchored 5-digit postal code rejects non-matching strings via coerce_and_validate.
    let zip_schema = CompiledSchema::compile(json!({
        "type": "object",
        "properties": {
            "zip": { "type": "string", "pattern": "^[0-9]{5}$" },
            "code": { "type": "string", "pattern": "^(?:US|CA)-[A-Z]{2,3}\\d+$" },
            "word": { "type": "string", "pattern": "\\bcat\\b" },
            "tag": { "type": "string", "pattern": "[^\\s\\d]+" },
            "cn": { "type": "string", "pattern": "^[\\u4e00-\\u9fff]{2}$" }
        },
        "required": ["zip"]
    }))
    .expect("valid pattern schema should compile");

    assert!(
        zip_schema
            .coerce_and_validate(&json!({
                "zip": "94107",
                "code": "US-CA9",
                "word": "a cat!",
                "tag": "alpha_beta",
                "cn": "你好"
            }))
            .is_ok()
    );

    let err = zip_schema
        .coerce_and_validate(&json!({ "zip": "ABC" }))
        .expect_err("ABC must fail ^[0-9]{5}$");
    assert_eq!(err.issues[0].path, "zip");
    assert!(err.issues[0].message.contains("pattern"));

    assert!(zip_schema.validate(&json!({ "zip": "123456" })).is_err());
    assert!(
        zip_schema
            .validate(&json!({ "zip": "94107", "code": "UK-AA1" }))
            .is_err()
    );
    assert!(
        zip_schema
            .validate(&json!({ "zip": "94107", "word": "scatter" }))
            .is_err()
    );
    assert!(
        zip_schema
            .validate(&json!({ "zip": "94107", "tag": "123 456" }))
            .is_err()
    );
    assert!(
        zip_schema
            .validate(&json!({ "zip": "94107", "cn": "你a" }))
            .is_err()
    );

    // 2. Non-exponential Thompson NFA handles classic ReDoS pattern instantly.
    let redos = CompiledSchema::compile(json!({
        "type": "string",
        "pattern": "^(a+)+b$"
    }))
    .unwrap();
    assert!(redos.validate(&json!("aaaaaab")).is_ok());
    let evil = format!("{}c", "a".repeat(40));
    assert!(redos.validate(&json!(evil)).is_err());

    // 3. Invalid patterns and unsupported lookaround / backreference syntax fail at compile time,
    //    including inside `$defs` and nested properties.
    let bad_patterns = [
        "[unclosed",
        "(unclosed",
        "a{5,2}",
        "*leading",
        "(?=lookahead)",
        "(?!neg-lookahead)",
        "(?<=lookbehind)",
        "(?<!neg-lookbehind)",
        "(a)\\1",
        "\\2",
    ];
    for pat in bad_patterns {
        let nested = json!({
            "$defs": {
                "Field": { "type": "string", "pattern": pat }
            },
            "type": "object",
            "properties": {
                "f": { "$ref": "#/$defs/Field" }
            }
        });
        assert!(
            CompiledSchema::compile(nested).is_err(),
            "pattern `{pat}` should fail compile"
        );
    }
}
