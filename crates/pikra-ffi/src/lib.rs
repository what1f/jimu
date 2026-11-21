//! C-ABI core for PikraKit Swift SDK and native language bindings.
//!
//! Strictly decoupled from `pikra-http`, `reqwest`, `rustls`, and native TLS: HTTP streaming
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
pub use session::{
    PikraEventFn, PikraResultDoneFn, PikraSession, PikraStreamDoneFn, PikraVoidDoneFn,
};
pub use tools::{PikraToolCallbacks, PikraToolCancelFn, PikraToolCompletion, PikraToolExecuteFn};
pub use transport::{PikraHttpCallbacks, PikraHttpCancelFn, PikraHttpStartFn, PikraHttpStream};
pub use types::PikraStatus;

use bytes::Bytes;
use pikra_agent::AgentToolResult;
use pikra_ai::{Model, ThinkingLevel};
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
    session: *mut PikraSession,
    out_error: *mut *mut c_char,
) -> Result<&'a PikraSession, PikraStatus> {
    if session.is_null() {
        unsafe { write_out_string(out_error, "session handle must not be null") };
        return Err(PikraStatus::ErrInvalidArgument);
    }
    // SAFETY: Caller guarantees `session` is a live handle from `pikra_session_create`.
    Ok(unsafe { &*session })
}

/* ---- Memory Management ---- */

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_string_free(ptr: *mut c_char) {
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
pub unsafe extern "C" fn pikra_http_on_response(
    stream: *mut PikraHttpStream,
    status_code: u16,
    headers_json: *const c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if stream.is_null() {
            return PikraStatus::ErrInvalidArgument;
        }
        let headers: Vec<(String, String)> = match unsafe { read_opt_cstr(headers_json) } {
            Some(s) if !s.is_empty() => match serde_json::from_str(&s) {
                Ok(h) => h,
                Err(_) => return PikraStatus::ErrInvalidArgument,
            },
            _ => Vec::new(),
        };
        // SAFETY: `stream` is live until `pikra_http_finish` is called.
        let s = unsafe { &*stream };
        s.on_response(status_code, headers)
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_http_push_chunk(
    stream: *mut PikraHttpStream,
    data: *const u8,
    len: usize,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if stream.is_null() {
            return PikraStatus::ErrInvalidArgument;
        }
        let bytes = if len == 0 || data.is_null() {
            Bytes::new()
        } else {
            // SAFETY: Caller guarantees `data` is readable for `len` bytes.
            let slice = unsafe { std::slice::from_raw_parts(data, len) };
            Bytes::copy_from_slice(slice)
        };
        // SAFETY: `stream` is live until `pikra_http_finish` is called.
        let s = unsafe { &*stream };
        s.push_chunk(bytes)
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_http_finish(
    stream: *mut PikraHttpStream,
    error_kind: i32,
    error_message: *const c_char,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if stream.is_null() {
            return;
        }
        let msg = unsafe { read_opt_cstr(error_message) };
        // SAFETY: `pikra_http_finish` consumes ownership of `stream` exactly once.
        let boxed = unsafe { Box::from_raw(stream) };
        boxed.finish(error_kind, msg);
    }));
}

/* ---- Tool Completion Functions ---- */

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_tool_update(
    completion: *mut PikraToolCompletion,
    result_json: *const c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        if completion.is_null() {
            return PikraStatus::ErrInvalidArgument;
        }
        let Ok(json_str) = (unsafe { read_cstr(result_json, "result_json") }) else {
            return PikraStatus::ErrInvalidArgument;
        };
        let Ok(partial) = serde_json::from_str::<AgentToolResult>(json_str) else {
            return PikraStatus::ErrInvalidArgument;
        };
        // SAFETY: `completion` is live until `pikra_tool_complete` is called.
        let comp = unsafe { &*completion };
        comp.update(partial)
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_tool_complete(
    completion: *mut PikraToolCompletion,
    result_json: *const c_char,
    error_message: *const c_char,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if completion.is_null() {
            return;
        }
        // SAFETY: `pikra_tool_complete` consumes ownership of `completion` exactly once.
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
pub unsafe extern "C" fn pikra_session_create(
    config_json: *const c_char,
    http_callbacks: PikraHttpCallbacks,
    tool_callbacks: PikraToolCallbacks,
    out_error: *mut *mut c_char,
) -> *mut PikraSession {
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
            Ok(inner) => Box::into_raw(Box::new(PikraSession { inner })),
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                std::ptr::null_mut()
            }
        }
    }))
    .unwrap_or_else(|_| {
        unsafe { write_out_string(out_error, "panic inside pikra_session_create") };
        std::ptr::null_mut()
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_close(
    session: *mut PikraSession,
    ctx: *mut c_void,
    on_done: Option<PikraVoidDoneFn>,
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
        // SAFETY: Caller guarantees `session` is live until `pikra_session_destroy`.
        let s = unsafe { &*session };
        s.inner.close_async(SendCallbackCtx(ctx), cb);
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_destroy(session: *mut PikraSession) {
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
pub unsafe extern "C" fn pikra_session_prompt_start(
    session: *mut PikraSession,
    prompt_json: *const c_char,
    ctx: *mut c_void,
    on_event: Option<PikraEventFn>,
    on_done: Option<PikraStreamDoneFn>,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let (Some(ev_cb), Some(done_cb)) = (on_event, on_done) else {
            unsafe { write_out_string(out_error, "on_event and on_done must not be null") };
            return PikraStatus::ErrInvalidArgument;
        };
        let json_str = match unsafe { read_cstr(prompt_json, "prompt_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        let req: FfiPromptRequest = match serde_json::from_str(json_str) {
            Ok(r) => r,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid prompt JSON: {e}")) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        match s
            .inner
            .start_prompt(req, SendCallbackCtx(ctx), ev_cb, done_cb)
        {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_steer(
    session: *mut PikraSession,
    id: *const c_char,
    text: *const c_char,
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
        s.inner.steer(id_str.to_string(), text_str.to_string())
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_follow_up(
    session: *mut PikraSession,
    id: *const c_char,
    text: *const c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let id_str = match unsafe { read_cstr(id, "id") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        let text_str = match unsafe { read_cstr(text, "text") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        match s.inner.follow_up(id_str.to_string(), text_str.to_string()) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_interrupt(
    session: *mut PikraSession,
    ctx: *mut c_void,
    on_done: Option<PikraVoidDoneFn>,
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
pub unsafe extern "C" fn pikra_session_set_context(
    session: *mut PikraSession,
    name: *const c_char,
    text: *const c_char,
    out_changed: *mut bool,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let name_str = match unsafe { read_cstr(name, "name") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        let text_str = match unsafe { read_cstr(text, "text") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
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
                PikraStatus::Ok
            }
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_append_context(
    session: *mut PikraSession,
    text: *const c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let text_str = match unsafe { read_cstr(text, "text") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        match s.inner.append_context(text_str.to_string()) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_set_model(
    session: *mut PikraSession,
    model_json: *const c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let json_str = match unsafe { read_cstr(model_json, "model_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        let model: Model = match serde_json::from_str(json_str) {
            Ok(m) => m,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid model JSON: {e}")) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        match s.inner.set_model(model) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_set_thinking_level(
    session: *mut PikraSession,
    level_str: *const c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let raw = match unsafe { read_cstr(level_str, "level_str") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        let level: ThinkingLevel =
            match serde_json::from_value(serde_json::Value::String(raw.to_ascii_lowercase())) {
                Ok(l) => l,
                Err(e) => {
                    unsafe {
                        write_out_string(out_error, format!("invalid thinking_level `{raw}`: {e}"))
                    };
                    return PikraStatus::ErrInvalidArgument;
                }
            };
        match s.inner.set_thinking_level(level) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_set_fast(
    session: *mut PikraSession,
    fast: bool,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.set_fast(fast) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_set_api_key(
    session: *mut PikraSession,
    api_key: *const c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let opt_key = unsafe { read_opt_cstr(api_key) }.filter(|k| !k.is_empty());
        match s.inner.set_api_key(opt_key) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_upsert_tools(
    session: *mut PikraSession,
    tools_json: *const c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let json_str = match unsafe { read_cstr(tools_json, "tools_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        let specs: Vec<FfiToolSpec> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid tools JSON: {e}")) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        match s.inner.upsert_tools(specs) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_remove_tools(
    session: *mut PikraSession,
    names_json: *const c_char,
    out_removed: *mut usize,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let json_str = match unsafe { read_cstr(names_json, "names_json") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        let names: Vec<String> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, format!("invalid names JSON: {e}")) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        match s.inner.remove_tools(&names) {
            Ok(removed) => {
                if !out_removed.is_null() {
                    // SAFETY: Caller provided writable `out_removed`.
                    unsafe { *out_removed = removed };
                }
                PikraStatus::Ok
            }
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_compact(
    session: *mut PikraSession,
    ctx: *mut c_void,
    on_done: Option<PikraResultDoneFn>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(cb) = on_done else {
            return;
        };
        if session.is_null() {
            let err_c = CString::new("session handle must not be null").ok();
            cb(
                ctx,
                PikraStatus::ErrInvalidArgument,
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
pub unsafe extern "C" fn pikra_session_fork(
    session: *mut PikraSession,
    entry_id: *const c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let id_str = match unsafe { read_cstr(entry_id, "entry_id") } {
            Ok(v) => v,
            Err(e) => {
                unsafe { write_out_string(out_error, e) };
                return PikraStatus::ErrInvalidArgument;
            }
        };
        match s.inner.fork(id_str) {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_entries(
    session: *mut PikraSession,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.entries() {
            Ok(entries) => match serde_json::to_string(&entries) {
                Ok(json) => {
                    unsafe { write_out_string(out_json, json) };
                    PikraStatus::Ok
                }
                Err(e) => {
                    unsafe { write_out_string(out_error, format!("serialize entries: {e}")) };
                    PikraStatus::ErrInternal
                }
            },
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_info(
    session: *mut PikraSession,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.info() {
            Ok(info) => match serde_json::to_string(&info) {
                Ok(json) => {
                    unsafe { write_out_string(out_json, json) };
                    PikraStatus::Ok
                }
                Err(e) => {
                    unsafe { write_out_string(out_error, format!("serialize info: {e}")) };
                    PikraStatus::ErrInternal
                }
            },
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_reload_extensions(
    session: *mut PikraSession,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.reload_extensions() {
            Ok(()) => PikraStatus::Ok,
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_session_reload_skills(
    session: *mut PikraSession,
    out_changed: *mut bool,
    out_error: *mut *mut c_char,
) -> PikraStatus {
    catch_unwind(AssertUnwindSafe(|| {
        let s = match with_session(session, out_error) {
            Ok(s) => s,
            Err(code) => return code,
        };
        match s.inner.reload_skills() {
            Ok(changed) => {
                if !out_changed.is_null() {
                    // SAFETY: Caller provided writable `out_changed`.
                    unsafe { *out_changed = changed };
                }
                PikraStatus::Ok
            }
            Err((code, msg)) => {
                unsafe { write_out_string(out_error, msg) };
                code
            }
        }
    }))
    .unwrap_or(PikraStatus::ErrInternal)
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

fn escape_skill_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn format_discovered_skills_prompt(skills: &[FfiDiscoveredSkill]) -> String {
    let visible: Vec<&FfiDiscoveredSkill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = Vec::with_capacity(visible.len() * 6 + 3);
    lines.push(
        "Available skills. Use read to load a skill's SKILL.md when the task matches its description, before doing the work."
            .to_string(),
    );
    lines.push("<available_skills>".to_string());
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!(
            "    <name>{}</name>",
            escape_skill_xml(&skill.name)
        ));
        lines.push(format!(
            "    <description>{}</description>",
            escape_skill_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_skill_xml(&skill.location)
        ));
        lines.push(format!(
            "    <revision>{}</revision>",
            escape_skill_xml(&skill.revision)
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_discover_skills(
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
        let catalog = pikra_kit::SkillCatalog::discover(&root_paths);
        encode_skill_catalog(&catalog, true, out_error)
    }))
    .unwrap_or_else(|_| {
        unsafe { write_out_string(out_error, "panic inside pikra_discover_skills") };
        std::ptr::null_mut()
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pikra_discover_skills_with_config(
    config_json: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        let config = unsafe { read_cstr(config_json, "config_json") }.and_then(|json| {
            serde_json::from_str::<pikra_kit::SkillDiscoveryConfig>(json).map_err(|e| e.to_string())
        });
        match config {
            Ok(config) => encode_skill_catalog(
                &pikra_kit::SkillCatalog::discover_with_config(&config),
                false,
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
    catalog: &pikra_kit::SkillCatalog,
    legacy_prompt: bool,
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
    let prompt = if legacy_prompt {
        format_discovered_skills_prompt(&skills)
    } else {
        catalog.format_for_context().unwrap_or_default()
    };
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
