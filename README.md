# Pikra

Tiny, decoupled, zero-bloat embeddable headless agent library suite in Rust, with native Swift SDK (`PikraKit`) and JSONL-over-stdio RPC (`pikra --mode rpc`).

## Crates

- `pikra-schema`: JSON Schema builders, 3-pass loose tool-argument coercion, and validator.
- `pikra-ai`: Unified multi-provider streaming LLM API (`openai-responses`, `openai-completions`, `anthropic-messages`, `google-generative-ai`) with `HttpTransport` injection and prompt-cache preservation.
- `pikra-http`: Native `reqwest`/`rustls` streaming transport for Rust binaries.
- `pikra-agent`: Stateful in-memory agent turn loop, parallel tool scheduling, steering, and middleware hooks.
- `pikra-codemode`: Restricted QuickJS sandbox for model-written multi-tool orchestration.
- `pikra-session`: Append-only session entry tree, branching, deduplicated `<system-reminder>` context blocks, and prefix-preserving compaction.
- `pikra-js`: Dynamic QuickJS extension runtime for custom tools and lifecycle hooks.
- `pikra-tools`: Optional built-in local workspace tools (`read`, `write`, `edit`, `bash`, `find`).
- `pikra-kit`: Batteries-included headless agent session with skills, deferred tool search (`tool_search`/`call_tool`), codemode, and MCP client.
- `pikra-ffi`: C-ABI core for `PikraKit` Swift SDK and cross-language bindings.
- `apps/pikra-rpc`: Standalone JSONL RPC subprocess server.
