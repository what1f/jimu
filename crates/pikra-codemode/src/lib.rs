//! Standalone restricted async QuickJS sandbox for model-written multi-tool orchestration.

mod declarations;
mod identifier;
mod prelude;
mod sandbox;
mod source;
mod types;
mod vm;

pub use declarations::{
    DEFAULT_INPUT_SCHEMA_MAX_CHARS, render_declarations, render_tool_signature, schema_to_type,
};
pub use identifier::to_codemode_identifier;
pub use sandbox::CodemodeSandbox;
pub use source::{
    CODEMODE_OPTIONS_PREFIX, CODEMODE_SOURCE_GRAMMAR, CodemodeSourceError, CodemodeSourceOptions,
    MAX_OPTIONS_TIMEOUT_MS, ParsedCodemodeSource, parse_codemode_source,
};
pub use types::{
    BoxFuture, CodemodeCall, CodemodeCallStatus, CodemodeConfigError, CodemodeError,
    CodemodeErrorKind, CodemodeExecuteOptions, CodemodeOutputItem, CodemodeResult,
    CodemodeSandboxOptions, CodemodeStoreWrites, CodemodeTool, CodemodeToolContext,
    DEFAULT_MAX_STACK_SIZE, DEFAULT_MEMORY_LIMIT_BYTES, DEFAULT_TIMEOUT_MS, FnTool,
    MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS,
};
