//! C-ABI core for JimuKit Swift SDK and native language bindings.
//!
//! Strictly decoupled from `jimu-http`, `reqwest`, `rustls`, and native TLS: HTTP streaming
//! and host tools are bridged over C callbacks. Rust drives the agent loop,
//! multi-provider SSE/wire protocols, JSONL persistence, prompt-cache-stable compaction,
//! deferred tool gateway, codemode QuickJS sandbox, and JS extension runtime.

#![allow(clippy::missing_safety_doc)]

mod builtin_tools;
mod session;
mod tools;
mod transport;
mod types;

pub use builtin_tools::*;
pub use session::{JimuEventFn, JimuResultDoneFn, JimuSession, JimuStreamDoneFn, JimuVoidDoneFn};
pub use tools::{JimuToolCallbacks, JimuToolCancelFn, JimuToolCompletion, JimuToolExecuteFn};
pub use transport::{JimuHttpCallbacks, JimuHttpCancelFn, JimuHttpStartFn, JimuHttpStream};
pub use types::JimuStatus;

use bytes::Bytes;
use jimu_agent::AgentToolResult;
use jimu_ai::{Model, ThinkingLevel};
use session::{FfiSession, SendCallbackCtx};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use types::{FfiPromptRequest, FfiSessionConfig, FfiToolSpec};

unsafe fn write_out_string(out: *mut *mut c_char, value: impl Into<String>) {
    if out.is_null() {
        return;
    }
    let s = value.into().replace('\0', "");
    if let Ok(cstr) = CString::new(s) {
        // SAFETY: Caller supplied a valid writable pointer `out`.
        unsafe { *out = cstr.into_raw() };
    }
}

unsafe fn read_cstr<'a>(ptr: *const c_char, field: &str) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err(format!("{field} must not be null"));
    }
    // SAFETY: Caller guarantees `ptr` points to a valid NUL-terminated C string.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|e| format!("{field} is not valid UTF-8: {e}"))
}

unsafe fn read_opt_cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Caller guarantees `ptr` is null or a valid NUL-terminated C string.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(ToString::to_string)
}

fn with_session<'a>(
    session: *mut JimuSession,
    out_error: *mut *mut c_char,
) -> Result<&'a JimuSession, JimuStatus> {
    if session.is_null() {
        unsafe { write_out_string(out_error, "session handle must not be null") };
        return Err(JimuStatus::ErrInvalidArgument);
    }
    // SAFETY: Caller guarantees `session` is a live handle from `jimu_session_create`.
    Ok(unsafe { &*session })
}

/* ---- Memory Management ---- */

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_string_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` was allocated via `CString::into_raw` by this library.
    unsafe {
        let _ = CString::from_raw(ptr);
    }
}

/* ---- HTTP Stream Sink Functions ---- */

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_http_on_response(
    stream: *mut JimuHttpStream,
    status_code: u16,
    headers_json: *const c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if stream.is_null() {
            return JimuStatus::ErrInvalidArgument;
        }
        let headers: Vec<(String, String)> = match unsafe { read_opt_cstr(headers_json) } {
            Some(s) if !s.is_empty() => match serde_json::from_str(&s) {
                Ok(h) => h,
                Err(_) => return JimuStatus::ErrInvalidArgument,
            },
            _ => Vec::new(),
        };
        // SAFETY: `stream` is live until `jimu_http_finish` is called.
        let s = unsafe { &*stream };
        s.on_response(status_code, headers)
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_http_push_chunk(
    stream: *mut JimuHttpStream,
    data: *const u8,
    len: usize,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if stream.is_null() {
            return JimuStatus::ErrInvalidArgument;
        }
        let bytes = if len == 0 || data.is_null() {
            Bytes::new()
        } else {
            // SAFETY: Caller guarantees `data` is readable for `len` bytes.
            let slice = unsafe { std::slice::from_raw_parts(data, len) };
            Bytes::copy_from_slice(slice)
        };
        // SAFETY: `stream` is live until `jimu_http_finish` is called.
        let s = unsafe { &*stream };
        s.push_chunk(bytes)
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_http_finish(
    stream: *mut JimuHttpStream,
    error_kind: i32,
    error_message: *const c_char,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if stream.is_null() {
            return;
        }
        let msg = unsafe { read_opt_cstr(error_message) };
        // SAFETY: `jimu_http_finish` consumes ownership of `stream` exactly once.
        let boxed = unsafe { Box::from_raw(stream) };
        boxed.finish(error_kind, msg);
    }));
}

/* ---- Tool Completion Functions ---- */

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_tool_update(
    completion: *mut JimuToolCompletion,
    result_json: *const c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if completion.is_null() {
            return JimuStatus::ErrInvalidArgument;
        }
        let Ok(json_str) = (unsafe { read_cstr(result_json, "result_json") }) else {
            return JimuStatus::ErrInvalidArgument;
        };
        let Ok(partial) = serde_json::from_str::<AgentToolResult>(json_str) else {
            return JimuStatus::ErrInvalidArgument;
        };
        // SAFETY: `completion` is live until `jimu_tool_complete` is called.
        let comp = unsafe { &*completion };
        comp.update(partial)
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_tool_complete(
    completion: *mut JimuToolCompletion,
    result_json: *const c_char,
    error_message: *const c_char,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if completion.is_null() {
            return;
        }
        // SAFETY: `jimu_tool_complete` consumes ownership of `completion` exactly once.
        let boxed = unsafe { Box::from_raw(completion) };
        let err_msg = unsafe { read_opt_cstr(error_message) }.filter(|s| !s.is_empty());
        if let Some(err) = err_msg {
            boxed.complete(Err(err));
            return;
        }
        let res_str = match unsafe { read_cstr(result_json, "result_json") } {
            Ok(s) => s,
            Err(e) => {
                boxed.complete(Err(e));
                return;
            }
        };
        match serde_json::from_str::<AgentToolResult>(res_str) {
            Ok(res) => boxed.complete(Ok(res)),
            Err(e) => boxed.complete(Err(format!("invalid tool result JSON: {e}"))),
        }
    }));
}

/* ---- Session Lifecycle & Operations ---- */

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_create(
    config_json: *const c_char,
    http_callbacks: JimuHttpCallbacks,
    tool_callbacks: JimuToolCallbacks,
    out_error: *mut *mut c_char,
) -> *mut JimuSession {
    catch_unwind(AssertUnwindSafe(|| {
        let cfg_str = match unsafe { read_cstr(config_json, "config_json") } {
            Ok(s) => s,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return std::ptr::null_mut();
            }
        };
        let cfg: FfiSessionConfig = match serde_json::from_str(cfg_str) {
            Ok(c) => c,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid config JSON: {e}")) };
                return std::ptr::null_mut();
            }
        };
        match FfiSession::create(cfg, http_callbacks, tool_callbacks) {
            Ok(inner) => Box::into_raw(Box::new(JimuSession { inner })),
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                std::ptr::null_mut()
            }
        }
    }))
    .unwrap_or_else(|_| {
        unsafe { write_out_string(out_error, "panic inside jimu_session_create") };
        std::ptr::null_mut()
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_close(
    session: *mut JimuSession,
    ctx: *mut c_void,
    on_done: Option<JimuVoidDoneFn>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(cb) = on_done else {
            if !session.is_null() {
                // SAFETY: Caller guarantees `session` is live.
                unsafe { &*session }.inner.cancel_for_destroy();
            }
            return;
        };
        if session.is_null() {
            cb(ctx);
            return;
        }
        // SAFETY: Caller guarantees `session` is live until `jimu_session_destroy`.
        let s = unsafe { &*session };
        s.inner.close_async(SendCallbackCtx(ctx), cb);
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_destroy(session: *mut JimuSession) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if session.is_null() {
            return;
        }
        // SAFETY: Caller transfers ownership of `session` to destroy it once.
        let boxed = unsafe { Box::from_raw(session) };
        boxed.inner.cancel_for_destroy();
        drop(boxed);
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_prompt_start(
    session: *mut JimuSession,
    prompt_json: *const c_char,
    ctx: *mut c_void,
    on_event: Option<JimuEventFn>,
    on_done: Option<JimuStreamDoneFn>,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let (Some(ev_cb), Some(done_cb)) = (on_event, on_done) else {
            unsafe { write_out_string(out_error, "on_event and on_done must not be null") };
            return JimuStatus::ErrInvalidArgument;
        };
        let json_str = match unsafe { read_cstr(prompt_json, "prompt_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        let req: FfiPromptRequest = match serde_json::from_str(json_str) {
            Ok(r) => r,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid prompt JSON: {e}")) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match s
            .inner
            .start_prompt(req, SendCallbackCtx(ctx), ev_cb, done_cb)
        {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_steer(
    session: *mut JimuSession,
    id: *const c_char,
    text: *const c_char,
    is_meta: bool,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        if session.is_null() {
            return false;
        }
        let Ok(id_str) = (unsafe { read_cstr(id, "id") }) else {
            return false;
        };
        let Ok(text_str) = (unsafe { read_cstr(text, "text") }) else {
            return false;
        };
        // SAFETY: Caller guarantees `session` is live.
        let s = unsafe { &*session };
        s.inner
            .steer(id_str.to_string(), text_str.to_string(), is_meta)
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_follow_up(
    session: *mut JimuSession,
    id: *const c_char,
    text: *const c_char,
    is_meta: bool,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let id_str = match unsafe { read_cstr(id, "id") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        let text_str = match unsafe { read_cstr(text, "text") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match s
            .inner
            .follow_up(id_str.to_string(), text_str.to_string(), is_meta)
        {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_interrupt(
    session: *mut JimuSession,
    ctx: *mut c_void,
    on_done: Option<JimuVoidDoneFn>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(cb) = on_done else {
            return;
        };
        if session.is_null() {
            cb(ctx);
            return;
        }
        // SAFETY: Caller guarantees `session` is live.
        let s = unsafe { &*session };
        s.inner.interrupt_async(SendCallbackCtx(ctx), cb);
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_set_context(
    session: *mut JimuSession,
    name: *const c_char,
    text: *const c_char,
    out_changed: *mut bool,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let name_str = match unsafe { read_cstr(name, "name") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        let text_str = match unsafe { read_cstr(text, "text") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match s
            .inner
            .set_context(name_str.to_string(), text_str.to_string())
        {
            Ok(changed) => {
                if !out_changed.is_null() {
                    // SAFETY: Caller provided writable `out_changed`.
                    unsafe { *out_changed = changed };
                }
                JimuStatus::Ok
            }
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_append_named_context(
    session: *mut JimuSession,
    name: *const c_char,
    text: *const c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    queue_session_context(session, text, out_error, |s, text| {
        let name = unsafe { read_cstr(name, "name") }
            .map_err(|err| (JimuStatus::ErrInvalidArgument, err))?;
        s.inner.append_named_context(name.to_string(), text)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_append_meta(
    session: *mut JimuSession,
    text: *const c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    queue_session_context(session, text, out_error, |s, text| {
        s.inner.append_meta(text)
    })
}

fn queue_session_context(
    session: *mut JimuSession,
    text: *const c_char,
    out_error: *mut *mut c_char,
    append: impl FnOnce(&JimuSession, String) -> Result<(), (JimuStatus, String)>,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let text_str = match unsafe { read_cstr(text, "text") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match append(s, text_str.to_string()) {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_set_model(
    session: *mut JimuSession,
    model_json: *const c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let json_str = match unsafe { read_cstr(model_json, "model_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        let model: Model = match serde_json::from_str(json_str) {
            Ok(m) => m,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid model JSON: {e}")) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match s.inner.set_model(model) {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_set_thinking_level(
    session: *mut JimuSession,
    level_str: *const c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let raw = match unsafe { read_cstr(level_str, "level_str") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        let level: ThinkingLevel =
            match serde_json::from_value(serde_json::Value::String(raw.to_ascii_lowercase())) {
                Ok(l) => l,
                Err(e) => {
                    unsafe {
                        write_out_string(out_error, format!("invalid thinking_level `{raw}`: {e}"))
                    };
                    return JimuStatus::ErrInvalidArgument;
                }
            };
        match s.inner.set_thinking_level(level) {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_set_fast(
    session: *mut JimuSession,
    fast: bool,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.set_fast(fast) {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_set_api_key(
    session: *mut JimuSession,
    api_key: *const c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let opt_key = unsafe { read_opt_cstr(api_key) }.filter(|k| !k.is_empty());
        match s.inner.set_api_key(opt_key) {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_upsert_tools(
    session: *mut JimuSession,
    tools_json: *const c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let json_str = match unsafe { read_cstr(tools_json, "tools_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        let specs: Vec<FfiToolSpec> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid tools JSON: {e}")) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match s.inner.upsert_tools(specs) {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_remove_tools(
    session: *mut JimuSession,
    names_json: *const c_char,
    out_removed: *mut usize,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let json_str = match unsafe { read_cstr(names_json, "names_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        let names: Vec<String> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid names JSON: {e}")) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match s.inner.remove_tools(&names) {
            Ok(removed) => {
                if !out_removed.is_null() {
                    // SAFETY: Caller provided writable `out_removed`.
                    unsafe { *out_removed = removed };
                }
                JimuStatus::Ok
            }
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_compact(
    session: *mut JimuSession,
    ctx: *mut c_void,
    on_done: Option<JimuResultDoneFn>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(cb) = on_done else {
            return;
        };
        if session.is_null() {
            let err_c = CString::new("session handle must not be null").ok();
            cb(
                ctx,
                JimuStatus::ErrInvalidArgument,
                std::ptr::null(),
                err_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            );
            return;
        }
        // SAFETY: Caller guarantees `session` is live.
        let s = unsafe { &*session };
        s.inner.compact_async(SendCallbackCtx(ctx), cb);
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_fork(
    session: *mut JimuSession,
    entry_id: *const c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let id_str = match unsafe { read_cstr(entry_id, "entry_id") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return JimuStatus::ErrInvalidArgument;
            }
        };
        match s.inner.fork(id_str) {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_entries(
    session: *mut JimuSession,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.entries() {
            Ok(entries) => match serde_json::to_string(&entries) {
                Ok(json) => {
                    unsafe { write_out_string(out_json, json) };
                    JimuStatus::Ok
                }
                Err(e) => {
                    unsafe { write_out_string(out_error, format!("serialize entries: {e}")) };
                    JimuStatus::ErrInternal
                }
            },
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_info(
    session: *mut JimuSession,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.info() {
            Ok(info) => match serde_json::to_string(&info) {
                Ok(json) => {
                    unsafe { write_out_string(out_json, json) };
                    JimuStatus::Ok
                }
                Err(e) => {
                    unsafe { write_out_string(out_error, format!("serialize info: {e}")) };
                    JimuStatus::ErrInternal
                }
            },
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_reload_extensions(
    session: *mut JimuSession,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.reload_extensions() {
            Ok(()) => JimuStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_reload_skills(
    session: *mut JimuSession,
    out_changed: *mut bool,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    session_reload(session, out_changed, out_error, FfiSession::reload_skills)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_session_reload_instructions(
    session: *mut JimuSession,
    out_changed: *mut bool,
    out_error: *mut *mut c_char,
) -> JimuStatus {
    session_reload(
        session,
        out_changed,
        out_error,
        FfiSession::reload_instructions,
    )
}

fn session_reload(
    session: *mut JimuSession,
    out_changed: *mut bool,
    out_error: *mut *mut c_char,
    reload: impl FnOnce(&FfiSession) -> Result<bool, (JimuStatus, String)>,
) -> JimuStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match reload(&s.inner) {
            Ok(changed) => {
                if !out_changed.is_null() {
                    // SAFETY: Caller provided writable `out_changed`.
                    unsafe { *out_changed = changed };
                }
                JimuStatus::Ok
            }
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(JimuStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_discover_instructions(
    config_json: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        let discover = || {
            let json = unsafe { read_cstr(config_json, "config_json") }?;
            let config = serde_json::from_str(json).map_err(|error| error.to_string())?;
            let catalog = jimu_kit::InstructionCatalog::discover(&config);
            let entries: Vec<_> = catalog
                .entries()
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "path": entry.path.to_string_lossy(),
                        "content": entry.content,
                        "truncated": entry.truncated,
                    })
                })
                .collect();
            let json = serde_json::json!({
                "entries": entries,
                "total_bytes": catalog.total_bytes(),
                "truncated": catalog.truncated(),
                "prompt": catalog.format_for_context(),
            });
            CString::new(json.to_string()).map_err(|error| error.to_string())
        };
        match discover() {
            Ok(json) => json.into_raw(),
            Err(error) => {
                unsafe { write_out_string(out_error, error) };
                std::ptr::null_mut()
            }
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

/* ---- Skill Discovery ---- */

#[derive(serde::Serialize)]
struct FfiDiscoveredSkill {
    name: String,
    description: String,
    location: String,
    disable_model_invocation: bool,
    revision: String,
}

#[derive(serde::Serialize)]
struct FfiSkillDiscoveryResponse {
    skills: Vec<FfiDiscoveredSkill>,
    prompt: String,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_discover_skills(
    roots_json: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        let json_str = match unsafe { read_cstr(roots_json, "roots_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return std::ptr::null_mut();
            }
        };
        let raw_roots: Vec<String> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid roots JSON: {e}")) };
                return std::ptr::null_mut();
            }
        };

        let root_paths: Vec<std::path::PathBuf> = raw_roots
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect();
        let catalog = jimu_kit::SkillCatalog::discover(&root_paths);
        encode_skill_catalog(&catalog, out_error)
    }))
    .unwrap_or_else(|_| {
        unsafe { write_out_string(out_error, "panic inside jimu_discover_skills") };
        std::ptr::null_mut()
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jimu_discover_skills_with_config(
    config_json: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        let config = unsafe { read_cstr(config_json, "config_json") }.and_then(|json| {
            serde_json::from_str::<jimu_kit::SkillDiscoveryConfig>(json).map_err(|e| e.to_string())
        });
        match config {
            Ok(config) => encode_skill_catalog(
                &jimu_kit::SkillCatalog::discover_with_config(&config),
                out_error,
            ),
            Err(error) => {
                unsafe { write_out_string(out_error, error) };
                std::ptr::null_mut()
            }
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

fn encode_skill_catalog(
    catalog: &jimu_kit::SkillCatalog,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    let skills: Vec<_> = catalog
        .skills()
        .iter()
        .map(|skill| FfiDiscoveredSkill {
            name: skill.name.clone(),
            description: skill.description.clone(),
            location: skill.file_path.to_string_lossy().into_owned(),
            disable_model_invocation: skill.disable_model_invocation,
            revision: skill.revision.clone(),
        })
        .collect();
    let prompt = catalog.format_for_context().unwrap_or_default();
    let response = FfiSkillDiscoveryResponse { skills, prompt };
    match serde_json::to_vec(&response)
        .map_err(|error| error.to_string())
        .and_then(|json| CString::new(json).map_err(|error| error.to_string()))
    {
        Ok(json) => json.into_raw(),
        Err(error) => {
            unsafe { write_out_string(out_error, error) };
            std::ptr::null_mut()
        }
    }
}
