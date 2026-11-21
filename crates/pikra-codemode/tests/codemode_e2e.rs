use futures_channel::oneshot;
use futures_executor::block_on;
use pikra_codemode::{
    CodemodeCallStatus, CodemodeErrorKind, CodemodeExecuteOptions, CodemodeOutputItem,
    CodemodeSandbox, CodemodeSandboxOptions, FnTool, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS,
    parse_codemode_source, to_codemode_identifier,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const TINY_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

#[test]
fn multi_tool_orchestration_output_isolation_and_exit() {
    block_on(async {
        let mut sandbox = CodemodeSandbox::new(CodemodeSandboxOptions::default());

        sandbox
            .register_tool(Arc::new(FnTool::new(
                "my-api",
                "Doubles a number or fails on negative input",
                |args, _ctx| async move {
                    let n = args.get("n").and_then(Value::as_i64).unwrap_or(0);
                    if n < 0 {
                        return Err(format!("negative input: {n}"));
                    }
                    Ok(json!({ "doubled": n * 2 }))
                },
            )))
            .unwrap();

        sandbox
            .register_tool(Arc::new(FnTool::new(
                "join-tag",
                "Appends a tag to a label",
                |args, _ctx| async move {
                    let label = args.get("label").and_then(Value::as_str).unwrap_or("");
                    Ok(json!(format!("{label}!")))
                },
            )))
            .unwrap();

        assert_eq!(to_codemode_identifier("my-api"), "my_api");

        let script = format!(
            r#"
const isolated = {{
    fetch: typeof fetch,
    process: typeof process,
    require: typeof require,
    setTimeout: typeof setTimeout,
    frozenTools: Object.isFrozen(ALL_TOOLS),
    allTools: ALL_TOOLS.map(t => ({{ name: t.name, rawName: t.rawName, description: t.description }})),
}};

// Parallel orchestration via Promise.all using both identifier and raw-name lookup
const parallel = await Promise.all([
    tools.my_api({{ n: 3 }}),
    tools["my-api"]({{ n: 7 }}),
]);

// Sequential loop
const tagged = [];
for (const item of parallel) {{
    tagged.push(await tools.join_tag({{ label: String(item.doubled) }}));
}}

// Tool error handling with try/catch
let caughtToolError = null;
try {{
    await tools.my_api({{ n: -5 }});
}} catch (err) {{
    caughtToolError = err.message;
}}

// Output helpers
text("summary line");
text({{ total: parallel[0].doubled + parallel[1].doubled }});
console.log("debug", 42);

// image() accepts data URI, {{ type: "image", data, mimeType }}, and {{ data, mime_type }}
image("data:image/png;base64,{TINY_PNG_B64}");
image({{ type: "image", data: "{TINY_PNG_B64}", mimeType: "image/png" }});
image({{ data: "{TINY_PNG_B64}", mime_type: "image/png" }});

// image() rejects remote https:// URLs
let remoteRejected = false;
try {{
    image("https://example.com/pic.png");
}} catch (err) {{
    remoteRejected = err instanceof Error && /remote/i.test(err.message);
}}

return {{ isolated, tagged, caughtToolError, remoteRejected }};
"#
        );

        let result = sandbox
            .execute(&script, CodemodeExecuteOptions::default())
            .await;

        assert!(result.ok, "unexpected error: {:?}", result.error);
        let value = result.value.expect("expected return value");
        assert_eq!(value["isolated"]["fetch"], "undefined");
        assert_eq!(value["isolated"]["process"], "undefined");
        assert_eq!(value["isolated"]["require"], "undefined");
        assert_eq!(value["isolated"]["setTimeout"], "undefined");
        assert_eq!(value["isolated"]["frozenTools"], true);
        assert_eq!(value["isolated"]["allTools"][0]["name"], "my_api");
        assert_eq!(value["isolated"]["allTools"][0]["rawName"], "my-api");
        assert_eq!(value["tagged"], json!(["6!", "14!"]));
        assert_eq!(value["caughtToolError"], "negative input: -5");
        assert_eq!(value["remoteRejected"], true);

        // Verify recorded nested tool calls (2 ok my-api, 2 ok join-tag, 1 error my-api)
        assert_eq!(result.calls.len(), 5);
        assert_eq!(result.calls[0].name, "my-api");
        assert!(!result.calls[0].is_error);
        assert_eq!(result.calls[0].status, CodemodeCallStatus::Ok);
        assert_eq!(result.calls[4].name, "my-api");
        assert!(result.calls[4].is_error);
        assert_eq!(result.calls[4].status, CodemodeCallStatus::Error);

        // Verify output items
        assert_eq!(
            result.output,
            vec![
                CodemodeOutputItem::Text {
                    text: "summary line".to_string(),
                    console: false,
                },
                CodemodeOutputItem::Text {
                    text: r#"{"total":20}"#.to_string(),
                    console: false,
                },
                CodemodeOutputItem::Text {
                    text: "debug 42".to_string(),
                    console: true,
                },
                CodemodeOutputItem::Image {
                    data: TINY_PNG_B64.to_string(),
                    mime_type: "image/png".to_string(),
                },
                CodemodeOutputItem::Image {
                    data: TINY_PNG_B64.to_string(),
                    mime_type: "image/png".to_string(),
                },
                CodemodeOutputItem::Image {
                    data: TINY_PNG_B64.to_string(),
                    mime_type: "image/png".to_string(),
                },
            ]
        );

        // Early exit() succeeds with ok: true, keeps accumulated output & store_writes, skips tail
        let exit_res = sandbox
            .execute(
                r#"
text("before exit");
store("saved", { step: 1 });
exit();
text("after exit");
store("ignored", true);
"#,
                CodemodeExecuteOptions::default(),
            )
            .await;

        assert!(exit_res.ok, "exit() should succeed: {:?}", exit_res.error);
        assert_eq!(exit_res.value, None);
        assert_eq!(
            exit_res.output,
            vec![CodemodeOutputItem::Text {
                text: "before exit".to_string(),
                console: false,
            }]
        );
        assert_eq!(exit_res.store_writes.set.get("saved"), Some(&json!({ "step": 1 })));
        assert!(!exit_res.store_writes.set.contains_key("ignored"));
    });
}

#[test]
fn options_header_line_numbers_and_transactional_store() {
    block_on(async {
        // 1. Header parsing validation
        assert!(parse_codemode_source("   \n  ").is_err());
        assert!(parse_codemode_source("// @options: {\"unknown\": 1}\nreturn 1;").is_err());
        assert!(parse_codemode_source("// @options: not-json\nreturn 1;").is_err());
        assert!(parse_codemode_source("// @options: {\"timeout_ms\": 1000}\n").is_err());
        assert!(parse_codemode_source("const a = 1;\n// @options: {\"timeout_ms\": 1000}").is_err());

        let raw_source = "// @options: {\"max_output_tokens\": 2000, \"timeout_ms\": 30000}\nconst x = 1;\nconst y = x + 1;\nthrow new Error(\"boom on line 4\");";
        let parsed = parse_codemode_source(raw_source).expect("valid header");
        assert_eq!(parsed.options.max_output_tokens, Some(2000));
        assert_eq!(parsed.options.timeout_ms, Some(30000));
        assert!(parsed.code.starts_with('\n'));

        let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions::default());

        // 2. Stack trace line number preservation (throw is on line 4 of original input)
        let err_res = sandbox
            .execute(&parsed.code, CodemodeExecuteOptions::default())
            .await;
        assert!(!err_res.ok);
        let err = err_res.error.expect("expected script error");
        assert_eq!(err.kind, CodemodeErrorKind::Script);
        assert_eq!(err.message, "boom on line 4");
        let stack = err.stack.expect("expected stack trace");
        assert!(
            stack.contains("codemode.js:4"),
            "expected line 4 in stack, got: {stack}"
        );

        // 3. store/load copy isolation, deletion, size limits, and rollback on failure
        let mut host_store = BTreeMap::new();
        host_store.insert("profile".to_string(), json!({ "tags": ["a"] }));
        host_store.insert("ephemeral".to_string(), json!("remove-me"));

        let store_script = format!(
            r#"
const first = load("profile");
first.tags.push("mutated-locally");
const second = load("profile");

store("counter", 42);
store("ephemeral", undefined);

let valueOverflow = false;
try {{
    store("too_big", "x".repeat({MAX_STORE_VALUE_CHARS} + 10));
}} catch (e) {{
    valueOverflow = e instanceof RangeError;
}}

let totalOverflow = false;
try {{
    const chunk = "a".repeat(200000);
    for (let i = 0; i < 6; i++) {{
        store("chunk_" + i, chunk);
    }}
}} catch (e) {{
    totalOverflow = e instanceof RangeError;
}}
// Clean up temporary chunks so final writes stay tidy
for (let i = 0; i < 6; i++) store("chunk_" + i, undefined);

return {{
    isolatedTags: second.tags,
    valueOverflow,
    totalOverflow,
    maxTotal: {MAX_STORE_TOTAL_CHARS},
}};
"#
        );

        let res = sandbox
            .execute(
                &store_script,
                CodemodeExecuteOptions {
                    store: host_store.clone(),
                    ..Default::default()
                },
            )
            .await;
        assert!(res.ok, "store test failed: {:?}", res.error);
        let val = res.value.expect("return value");
        assert_eq!(val["isolatedTags"], json!(["a"]));
        assert_eq!(val["valueOverflow"], true);
        assert_eq!(val["totalOverflow"], true);

        res.store_writes.apply_to(&mut host_store);
        assert_eq!(host_store.get("counter"), Some(&json!(42)));
        assert!(!host_store.contains_key("ephemeral"));

        // Rollback on throw: writes made before throwing must be discarded
        let rollback_res = sandbox
            .execute(
                r#"
store("counter", 999);
store("new_key", "should-not-persist");
throw new Error("abort transaction");
"#,
                CodemodeExecuteOptions {
                    store: host_store.clone(),
                    ..Default::default()
                },
            )
            .await;
        assert!(!rollback_res.ok);
        assert!(rollback_res.store_writes.is_empty());
    });
}

#[test]
fn safety_guards_timeout_abort_oom_and_unsettled_promise() {
    block_on(async {
        // 1. Tight CPU loop interrupted cleanly by timeout_ms via interrupt handler
        let fast_timeout_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            timeout_ms: Some(150),
            memory_limit_bytes: 16 * 1024 * 1024,
            ..Default::default()
        });
        let loop_res = fast_timeout_sandbox
            .execute("while (true) {}", CodemodeExecuteOptions::default())
            .await;
        assert!(!loop_res.ok);
        assert_eq!(
            loop_res.error.as_ref().map(|e| e.kind),
            Some(CodemodeErrorKind::Timeout)
        );

        // 2. Cooperative abort flag interrupts a running script cleanly without sleep
        let abort_flag = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));

        let mut abort_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            timeout_ms: Some(60_000),
            ..Default::default()
        });
        abort_sandbox
            .register_tool(Arc::new(FnTool::new(
                "wait-forever",
                "Signals readiness then waits for cancellation",
                move |_args, _ctx| {
                    let started_tx = started_tx.clone();
                    async move {
                        if let Some(tx) = started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        futures_util::future::pending::<Result<Value, String>>().await
                    }
                },
            )))
            .unwrap();

        let abort_for_trigger = abort_flag.clone();
        let exec_fut = abort_sandbox.execute(
            "await tools.wait_forever();",
            CodemodeExecuteOptions {
                abort: Some(abort_flag),
                ..Default::default()
            },
        );
        let trigger_fut = async move {
            started_rx.await.unwrap();
            abort_for_trigger.store(true, Ordering::Release);
        };
        let (abort_res, ()) = futures_util::future::join(exec_fut, trigger_fut).await;
        assert!(!abort_res.ok);
        assert_eq!(
            abort_res.error.as_ref().map(|e| e.kind),
            Some(CodemodeErrorKind::Aborted)
        );

        // 3. Out-of-memory caught cleanly by memory_limit_bytes without crashing host
        let oom_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            timeout_ms: Some(10_000),
            memory_limit_bytes: 8 * 1024 * 1024,
            ..Default::default()
        });
        let oom_res = oom_sandbox
            .execute(
                r#"const big = new Array(1e8).fill("x"); return big.length;"#,
                CodemodeExecuteOptions::default(),
            )
            .await;
        assert!(!oom_res.ok);
        assert_eq!(
            oom_res.error.as_ref().map(|e| e.kind),
            Some(CodemodeErrorKind::OutOfMemory),
            "expected OutOfMemory, got: {:?}",
            oom_res.error
        );

        // 4. Dangling promise fails immediately as UnsettledPromise without waiting for 60s timeout
        let dangle_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            timeout_ms: Some(60_000),
            ..Default::default()
        });
        let dangle_res = dangle_sandbox
            .execute(
                "await new Promise(() => {});",
                CodemodeExecuteOptions::default(),
            )
            .await;
        assert!(!dangle_res.ok);
        assert_eq!(
            dangle_res.error.as_ref().map(|e| e.kind),
            Some(CodemodeErrorKind::UnsettledPromise),
            "expected UnsettledPromise, got: {:?}",
            dangle_res.error
        );
    });
}

#[test]
fn review_regressions_stack_drop_order_budget_microtasks_and_prelude() {
    block_on(async {
        // 1. Stack overflow protection: extreme `max_stack_size` (0 or 64 MiB) is clamped safely
        //    so infinite JS recursion fails as a Script error instead of crashing the host thread.
        for bad_stack in [0usize, 64 * 1024 * 1024] {
            let stack_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
                timeout_ms: Some(5_000),
                max_stack_size: bad_stack,
                ..Default::default()
            });
            let res = stack_sandbox
                .execute(
                    "function recurse() { return recurse(); } return recurse();",
                    CodemodeExecuteOptions::default(),
                )
                .await;
            assert!(!res.ok);
            assert_eq!(
                res.error.as_ref().map(|e| e.kind),
                Some(CodemodeErrorKind::Script),
                "max_stack_size={bad_stack} should catch stack overflow cleanly, got: {:?}",
                res.error
            );
        }

        // 2. CancelOnDrop order: dropping `execute()` early sets `ctx.abort` BEFORE dropping in-flight tool futures.
        let saw_aborted_on_drop = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));

        struct DropObserver {
            abort: Arc<AtomicBool>,
            saw_aborted: Arc<AtomicBool>,
        }
        impl Drop for DropObserver {
            fn drop(&mut self) {
                self.saw_aborted
                    .store(self.abort.load(Ordering::Acquire), Ordering::Release);
            }
        }

        let mut drop_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            timeout_ms: Some(60_000),
            ..Default::default()
        });
        {
            let saw_aborted_on_drop = saw_aborted_on_drop.clone();
            let started_tx = started_tx.clone();
            drop_sandbox
                .register_tool(Arc::new(FnTool::new(
                    "hold",
                    "Holds until dropped and records abort flag state on Drop",
                    move |_args, ctx| {
                        let saw_aborted_on_drop = saw_aborted_on_drop.clone();
                        let started_tx = started_tx.clone();
                        async move {
                            let _observer = DropObserver {
                                abort: ctx.abort.clone(),
                                saw_aborted: saw_aborted_on_drop,
                            };
                            if let Some(tx) = started_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            futures_util::future::pending::<Result<Value, String>>().await
                        }
                    },
                )))
                .unwrap();
        }

        {
            let exec_fut = drop_sandbox.execute(
                "await tools.hold();",
                CodemodeExecuteOptions::default(),
            );
            futures_util::pin_mut!(exec_fut);
            let race = futures_util::future::select(exec_fut, started_rx).await;
            assert!(
                matches!(race, futures_util::future::Either::Right(_)),
                "hold tool should signal readiness before execute completes"
            );
            // Dropping `race` drops `exec_fut` early while `tools.hold()` is still in flight.
        }
        assert!(
            saw_aborted_on_drop.load(Ordering::Acquire),
            "CancelOnDrop must set tool abort flag BEFORE dropping in-flight tool futures"
        );

        // 3. Eager first-poll: fire-and-forget tool call starts deterministically before script returns.
        let side_effect_ran = Arc::new(AtomicBool::new(false));
        let mut eager_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions::default());
        {
            let flag = side_effect_ran.clone();
            eager_sandbox
                .register_tool(Arc::new(FnTool::new(
                    "fire",
                    "Marks side effect immediately",
                    move |_args, _ctx| {
                        let flag = flag.clone();
                        async move {
                            flag.store(true, Ordering::Release);
                            Ok(json!(true))
                        }
                    },
                )))
                .unwrap();
        }
        let eager_res = eager_sandbox
            .execute(
                r#"tools.fire(); return "done";"#,
                CodemodeExecuteOptions::default(),
            )
            .await;
        assert!(eager_res.ok, "unexpected error: {:?}", eager_res.error);
        assert!(
            side_effect_ran.load(Ordering::Acquire),
            "fire-and-forget tool call must be eagerly polled"
        );

        // 4. Microtask starvation prevention: tight `await Promise.resolve()` loop must not starve ready tool completions.
        let mut microtask_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            timeout_ms: Some(5_000),
            ..Default::default()
        });
        microtask_sandbox
            .register_tool(Arc::new(FnTool::new(
                "fast",
                "Returns immediately",
                |_args, _ctx| async move { Ok(json!("ok")) },
            )))
            .unwrap();
        let mt_res = microtask_sandbox
            .execute(
                r#"
let settled = false;
tools.fast().then(() => { settled = true; });
while (!settled) {
    await Promise.resolve();
}
return "unstarved";
"#,
                CodemodeExecuteOptions::default(),
            )
            .await;
        assert!(mt_res.ok, "microtask loop should not starve tool completion: {:?}", mt_res.error);
        assert_eq!(mt_res.value, Some(json!("unstarved")));

        // 5. In-flight tool call count & queued argument payload budget enforcement.
        let mut budget_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            timeout_ms: Some(5_000),
            ..Default::default()
        });
        budget_sandbox
            .register_tool(Arc::new(FnTool::new(
                "stall",
                "Never finishes unless cancelled",
                |_args, _ctx| async move {
                    futures_util::future::pending::<Result<Value, String>>().await
                },
            )))
            .unwrap();
        let flood_res = budget_sandbox
            .execute(
                "for (let i = 0; i < 2000; i++) { tools.stall({ i }); } return 1;",
                CodemodeExecuteOptions::default(),
            )
            .await;
        assert!(!flood_res.ok);
        assert_eq!(
            flood_res.error.as_ref().map(|e| e.kind),
            Some(CodemodeErrorKind::Script),
            "call flood should fail cleanly with RangeError, got: {:?}",
            flood_res.error
        );
        assert_eq!(
            flood_res.error.as_ref().and_then(|e| e.name.as_deref()),
            Some("RangeError")
        );

        let arg_flood_res = budget_sandbox
            .execute(
                r#"const big = "x".repeat(600000); for (let i = 0; i < 40; i++) { tools.stall(big); } return 1;"#,
                CodemodeExecuteOptions::default(),
            )
            .await;
        assert!(!arg_flood_res.ok);
        assert_eq!(
            arg_flood_res.error.as_ref().and_then(|e| e.name.as_deref()),
            Some("RangeError")
        );

        // 5b. Host-side deadline enforcement settles promptly when awaiting a pending tool.
        let deadline_res = budget_sandbox
            .execute(
                "await tools.stall();",
                CodemodeExecuteOptions {
                    timeout_ms: Some(50),
                    ..Default::default()
                },
            )
            .await;
        assert!(!deadline_res.ok);
        assert_eq!(
            deadline_res.error.as_ref().map(|e| e.kind),
            Some(CodemodeErrorKind::Timeout)
        );

        // 6. `image()` magic-byte validation & `text()` primitive formatting.
        let prelude_sandbox = CodemodeSandbox::new(CodemodeSandboxOptions::default());
        // Valid minimal JPEG (FF D8 FF E0) and WebP (RIFF....WEBP) headers in base64:
        let jpeg_b64 = "/9j/4AAQ";
        let gif_b64 = "R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==";
        let webp_b64 = "UklGRhIAAABXRUJQ";
        let prelude_script = format!(
            r#"
text(NaN);
text(Infinity);
text(1n);
text(true);
text(undefined);
text(Symbol("tag"));
text({{ k: 1 }});

// Auto-detect MIME from decoded magic bytes
image({{ data: "{TINY_PNG_B64}" }});
image({{ data: "{jpeg_b64}" }});
image({{ data: "{gif_b64}" }});
image({{ data: "{webp_b64}" }});

const rejects = [];
for (const bad of [
    "data:image/jpeg;base64,{TINY_PNG_B64}",
    {{ data: "{TINY_PNG_B64}", mimeType: "image/jpeg" }},
    "data:image/png;base64,aGVsbG8gd29ybGQ=",
    {{ data: "aGVsbG8gd29ybGQ=", mimeType: "image/png" }},
]) {{
    let threw = false;
    try {{
        image(bad);
    }} catch (e) {{
        threw = e instanceof TypeError;
    }}
    rejects.push(threw);
}}
return rejects;
"#
        );
        let prelude_res = prelude_sandbox
            .execute(&prelude_script, CodemodeExecuteOptions::default())
            .await;
        assert!(prelude_res.ok, "prelude regression test failed: {:?}", prelude_res.error);
        assert_eq!(prelude_res.value, Some(json!([true, true, true, true])));
        assert_eq!(
            prelude_res.output,
            vec![
                CodemodeOutputItem::Text { text: "NaN".to_string(), console: false },
                CodemodeOutputItem::Text { text: "Infinity".to_string(), console: false },
                CodemodeOutputItem::Text { text: "1".to_string(), console: false },
                CodemodeOutputItem::Text { text: "true".to_string(), console: false },
                CodemodeOutputItem::Text { text: "undefined".to_string(), console: false },
                CodemodeOutputItem::Text { text: "Symbol(tag)".to_string(), console: false },
                CodemodeOutputItem::Text { text: r#"{"k":1}"#.to_string(), console: false },
                CodemodeOutputItem::Image {
                    data: TINY_PNG_B64.to_string(),
                    mime_type: "image/png".to_string(),
                },
                CodemodeOutputItem::Image {
                    data: jpeg_b64.to_string(),
                    mime_type: "image/jpeg".to_string(),
                },
                CodemodeOutputItem::Image {
                    data: gif_b64.to_string(),
                    mime_type: "image/gif".to_string(),
                },
                CodemodeOutputItem::Image {
                    data: webp_b64.to_string(),
                    mime_type: "image/webp".to_string(),
                },
            ]
        );
    });
}
