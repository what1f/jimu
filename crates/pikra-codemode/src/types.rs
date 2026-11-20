//! Core types and traits for `pikra-codemode`.

use pikra_schema::Schema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

/// Boxed `Send` future alias used by [`CodemodeTool`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Default execution deadline per script run (5 minutes).
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Default QuickJS heap memory limit (32 MiB).
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 32 * 1024 * 1024;

/// Default QuickJS JS stack guard limit (512 KiB).
pub const DEFAULT_MAX_STACK_SIZE: usize = 512 * 1024;

/// Maximum JSON character length for a single `store(key, value)` entry (256 KiB).
pub const MAX_STORE_VALUE_CHARS: usize = 256 * 1024;

/// Maximum aggregate character length (keys + JSON values) across all `store()` entries (1 MiB).
pub const MAX_STORE_TOTAL_CHARS: usize = 1024 * 1024;

/// Maximum aggregate character length of script output (`text()`, `console.*`, `image()`).
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;

/// Maximum number of output items a single script execution may emit.
pub const MAX_OUTPUT_ITEMS: usize = 100_000;

/// Context handed to a host [`CodemodeTool`] invocation.
#[derive(Debug, Clone)]
pub struct CodemodeToolContext {
    /// Set to `true` if the execution finishes, times out, or is aborted while this tool runs.
    pub abort: Arc<AtomicBool>,
}

impl CodemodeToolContext {
    pub fn is_aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }
}

/// Async host tool exposed inside the QuickJS sandbox under `tools.<ident>` and `tools["raw"]`.
pub trait CodemodeTool: Send + Sync {
    /// Raw tool name (also exposed via `tools[name]`, and normalized via `to_codemode_identifier`).
    fn name(&self) -> &str;

    /// Optional human/model-facing description (shown in `ALL_TOOLS` and `render_declarations`).
    fn description(&self) -> &str {
        ""
    }

    /// Optional JSON Schema for the tool argument (used by `render_declarations`).
    fn input_schema(&self) -> Option<&Schema> {
        None
    }

    /// Optional JSON Schema for the tool return value (used by `render_declarations`).
    fn output_schema(&self) -> Option<&Schema> {
        None
    }

    /// Executes the tool asynchronously. Returning `Err(msg)` rejects the JS Promise with `new Error(msg)`.
    fn execute(
        &self,
        args: Value,
        ctx: CodemodeToolContext,
    ) -> BoxFuture<'_, Result<Value, String>>;
}

/// Ergonomic closure-backed [`CodemodeTool`] implementation.
pub struct FnTool<F> {
    name: String,
    description: String,
    input_schema: Option<Schema>,
    output_schema: Option<Schema>,
    handler: F,
}

impl<F, Fut> FnTool<F>
where
    F: Fn(Value, CodemodeToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, String>> + Send + 'static,
{
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        handler: F,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: None,
            output_schema: None,
            handler,
        }
    }

    pub fn with_input_schema(mut self, schema: Schema) -> Self {
        self.input_schema = Some(schema);
        self
    }

    pub fn with_output_schema(mut self, schema: Schema) -> Self {
        self.output_schema = Some(schema);
        self
    }
}

impl<F, Fut> CodemodeTool for FnTool<F>
where
    F: Fn(Value, CodemodeToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, String>> + Send + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Option<&Schema> {
        self.input_schema.as_ref()
    }

    fn output_schema(&self) -> Option<&Schema> {
        self.output_schema.as_ref()
    }

    fn execute(
        &self,
        args: Value,
        ctx: CodemodeToolContext,
    ) -> BoxFuture<'_, Result<Value, String>> {
        Box::pin((self.handler)(args, ctx))
    }
}

/// Sandbox configuration error (for example duplicate tool registration).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodemodeConfigError {
    #[error("tool \"{0}\" is already registered")]
    DuplicateTool(String),
}

/// Single output item emitted by `text()`, `console.*`, or `image()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodemodeOutputItem {
    Text { text: String, console: bool },
    Image { data: String, mime_type: String },
}

/// Settlement status of a nested host tool call made by the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodemodeCallStatus {
    Ok,
    Error,
    Cancelled,
}

/// Host-recorded audit entry for each nested `tools.*` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeCall {
    pub name: String,
    pub status: CodemodeCallStatus,
    pub is_error: bool,
    pub duration_ms: u64,
}

/// High-level failure category for an unsuccessful [`CodemodeResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodemodeErrorKind {
    /// Syntax error or uncaught JS exception thrown by the script.
    Script,
    /// Deadline expired while computing or awaiting a host tool.
    Timeout,
    /// Cooperative abort flag was set.
    Aborted,
    /// QuickJS memory limit (`memory_limit_bytes`) exceeded.
    OutOfMemory,
    /// Async wrapper Promise remained pending with zero in-flight host tool calls.
    UnsettledPromise,
    /// Internal VM/bridge failure outside the script's control.
    Sandbox,
}

/// Structured error returned when `CodemodeResult.ok == false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeError {
    pub kind: CodemodeErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

impl CodemodeError {
    pub fn script(
        name: Option<String>,
        message: impl Into<String>,
        stack: Option<String>,
    ) -> Self {
        Self {
            kind: CodemodeErrorKind::Script,
            name,
            message: message.into(),
            stack,
        }
    }

    pub fn timeout(timeout_ms: u64) -> Self {
        Self {
            kind: CodemodeErrorKind::Timeout,
            name: None,
            message: format!("Execution timed out after {timeout_ms} ms"),
            stack: None,
        }
    }

    pub fn aborted(message: impl Into<String>) -> Self {
        Self {
            kind: CodemodeErrorKind::Aborted,
            name: None,
            message: message.into(),
            stack: None,
        }
    }

    pub fn out_of_memory(message: impl Into<String>) -> Self {
        Self {
            kind: CodemodeErrorKind::OutOfMemory,
            name: Some("InternalError".to_string()),
            message: message.into(),
            stack: None,
        }
    }

    pub fn unsettled_promise() -> Self {
        Self {
            kind: CodemodeErrorKind::UnsettledPromise,
            name: Some("Error".to_string()),
            message: "The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here.".to_string(),
            stack: None,
        }
    }

    pub fn sandbox(message: impl Into<String>) -> Self {
        Self {
            kind: CodemodeErrorKind::Sandbox,
            name: None,
            message: message.into(),
            stack: None,
        }
    }
}

/// Transactional key-value mutations recorded via `store(key, value)` during a successful run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodemodeStoreWrites {
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
    #[serde(default)]
    pub delete: Vec<String>,
}

impl CodemodeStoreWrites {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.delete.is_empty()
    }

    /// Applies these writes onto a host-managed store map.
    pub fn apply_to(&self, store: &mut BTreeMap<String, Value>) {
        for key in &self.delete {
            store.remove(key);
        }
        for (key, value) in &self.set {
            store.insert(key.clone(), value.clone());
        }
    }
}

/// Final outcome of [`crate::CodemodeSandbox::execute`].
///
/// `execute` never returns `Err` for script errors, timeouts, aborts, OOM, or unsettled promises;
/// all outcomes are represented uniformly in `CodemodeResult`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodeResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CodemodeError>,
    pub output: Vec<CodemodeOutputItem>,
    pub calls: Vec<CodemodeCall>,
    pub store_writes: CodemodeStoreWrites,
}

impl CodemodeResult {
    pub(crate) fn ok(
        value: Option<Value>,
        output: Vec<CodemodeOutputItem>,
        calls: Vec<CodemodeCall>,
        store_writes: CodemodeStoreWrites,
    ) -> Self {
        Self {
            ok: true,
            value,
            error: None,
            output,
            calls,
            store_writes,
        }
    }

    pub(crate) fn err(
        error: CodemodeError,
        output: Vec<CodemodeOutputItem>,
        calls: Vec<CodemodeCall>,
    ) -> Self {
        Self {
            ok: false,
            value: None,
            error: Some(error),
            output,
            calls,
            store_writes: CodemodeStoreWrites::default(),
        }
    }
}

/// Sandbox-wide defaults for [`crate::CodemodeSandbox`].
#[derive(Debug, Clone)]
pub struct CodemodeSandboxOptions {
    /// Default hard deadline per execution in milliseconds (`None` disables the default deadline).
    pub timeout_ms: Option<u64>,
    /// QuickJS VM memory limit in bytes (default: 32 MiB).
    pub memory_limit_bytes: usize,
    /// QuickJS JS stack guard limit in bytes (default: 512 KiB).
    pub max_stack_size: usize,
}

impl Default for CodemodeSandboxOptions {
    fn default() -> Self {
        Self {
            timeout_ms: Some(DEFAULT_TIMEOUT_MS),
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            max_stack_size: DEFAULT_MAX_STACK_SIZE,
        }
    }
}

/// Per-execution overrides for [`crate::CodemodeSandbox::execute`].
#[derive(Debug, Clone, Default)]
pub struct CodemodeExecuteOptions {
    /// Overrides the sandbox's default timeout for this run (in milliseconds).
    pub timeout_ms: Option<u64>,
    /// Cooperative abort flag; setting it to `true` interrupts CPU loops and cancels pending tools.
    pub abort: Option<Arc<AtomicBool>>,
    /// Initial JSON key-value store visible to `load(key)` and `store(key, value)`.
    pub store: BTreeMap<String, Value>,
}
