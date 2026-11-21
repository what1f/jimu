# Pikra

Tiny, decoupled, zero-bloat embeddable headless agent library suite in Rust, with a zero-dependency Swift 6 SDK (`PikraKit`) and strict JSONL-over-stdio RPC (`pikra --mode rpc`).

## Architecture & Crates

Every crate in the workspace is independently usable with strict dependency boundaries (zero interactive TUI bloat; `pikra-ai` and `pikra-ffi` never pull in `reqwest`, `rustls`, or `tokio`):

- **`pikra-schema`**: JSON Schema builders, validator, and 3-pass copy-on-write LLM tool-argument coercion.
- **`pikra-ai`**: Unified multi-provider streaming LLM API (`openai-responses`, `openai-completions`, `anthropic-messages`, `google-generative-ai`) with `HttpTransport` injection, cross-provider message handoff, and byte-stable prompt-cache preservation.
- **`pikra-http`**: Optional native `reqwest`/`rustls` streaming transport with same-origin redirect guards for standalone Rust binaries.
- **`pikra-agent`**: Stateful turn loop, parallel/sequential tool scheduling with source-order persistence, `steer`/`follow_up`/`abort` teardown guarantees, and 5-hook `AgentMiddleware` pipeline.
- **`pikra-codemode`**: Restricted `rquickjs` async JS tool-orchestration sandbox (interpreter-only, no JIT for iOS App Store compliance, memory/stack/deadline/microtask guards).
- **`pikra-session`**: Append-only session entry tree (`MemorySessionStore` + `JsonlSessionStore`), `fork`, deduplicated `<system-reminder>` context blocks, base64-safe token accounting, and prefix-preserving auto-compaction.
- **`pikra-js`**: Dynamic QuickJS extension runtime for custom tools and control-plane hooks (`prepare_request`, `transform_context`, `before_tool_call`, `after_tool_call`, `finish_turn`) with O(1) bitmask skip.
- **`pikra-tools`**: Optional built-in local workspace tools (`read`, `write`, `edit`, `bash`, `find`) with dynamic `env_provider`, UTF-8-safe output truncation, and Unix process-group teardown.
- **`pikra-kit`**: Batteries-included headless session combining `Agent`, `SessionManager`, progressive `SkillCatalog`, deferred tool search (`tool_search` + `call_tool`), `exec` codemode bridge, and Streamable HTTP MCP client.
- **`pikra-ffi`**: Clean C-ABI core (`include/pikra.h`) driving Pikra over `futures_executor::ThreadPool` with host HTTP streaming backpressure and async tool VTables.
- **`apps/pikra-rpc`**: Standalone `pikra` CLI (`pikra ask` one-shot & `pikra --mode rpc` strict LF-delimited JSONL subprocess server).
- **`swift/` (`PikraKit`)**: Zero-external-dependency Swift 6 strict-concurrency `actor PikraAgent` SDK + `PikraCore.xcframework` with ephemeral `URLSession` streaming and `PikraToolApproval`.

## Quick Verification

```bash
# 1. Rust workspace E2E suite & Clippy
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# 2. Build PikraCore.xcframework & run Swift 6 PikraKit E2E suite
./swift/scripts/build-xcframework.sh
swift test --package-path swift

# 3. Build standalone RPC binary
cargo build --release -p pikra-rpc
```

See [`docs/architecture.md`](docs/architecture.md) and [`AGENTS.md`](AGENTS.md) for detailed design invariants.
