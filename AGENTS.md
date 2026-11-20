# Pikra Repository Rules

Pikra is a tiny, zero-bloat, decoupled, embeddable headless agent library suite in Rust, inspired by `@earendil-works/pi`, OpenAI `codex-rs`, and Claude Code.

## 1. Module Dependency Rules (Strictly Unidirectional)

Each crate MUST remain independently usable without pulling unnecessary upstream layers:
- `pikra-schema`: Standalone JSON Schema builder, 3-pass loose argument coercion, and model-friendly validator. Depends ONLY on `serde` / `serde_json`.
- `pikra-ai`: Unified multi-provider streaming LLM protocols (`openai-responses`, `openai-completions`, `anthropic-messages`, `google-generative-ai`), `TranscriptContext`, cross-provider message handoffs, usage/cost accounting, and `HttpTransport` trait. NO `tokio`, NO `reqwest`, NO TLS dependencies.
- `pikra-http`: Optional native `HttpTransport` implementation backed by `reqwest`/`rustls` for standalone Rust hosts and `pikra-rpc`. Never linked into Swift/Kotlin/Wasm FFI builds.
- `pikra-agent`: Pure in-memory stateful Agent Turn Loop, concurrent tool execution with source-order persistence, `steer`/`follow_up`/`abort` lifecycle, and composable middleware hooks (`prepare_request`, `transform_context`, `before_tool_call`, `after_tool_call`, `finish_turn`). Depends ONLY on `pikra-ai` and `pikra-schema`.
- `pikra-codemode`: Standalone restricted async QuickJS (`rquickjs`, interpreter-only, no JIT) sandbox exposing injected `tools.*`, `ALL_TOOLS`, `text()`, `image()`, `exit()`, `store/load` with memory and execution timeout limits. Depends ONLY on `pikra-schema` and `rquickjs`.
- `pikra-session`: Append-only session entry tree (`MemoryStore`, `JsonlStore`), branching (`fork`), named `<system-reminder>` context blocks (deduplicated by name+value, restored after compaction), and prefix-cache-preserving compaction.
- `pikra-js`: Dynamic QuickJS extension runtime for registering custom tools and turn/tool-level hooks (`reload` capable; never invoked on per-chunk streaming deltas).
- `pikra-tools`: Optional local system tools (`read`, `write`, `edit`, `bash`, `find`) with host-configurable `env` and `max_output_bytes`.
- `pikra-kit`: High-level headless composition (`AgentSession`, skill discovery, deferred tool gateway `tool_search`/`call_tool`, `pikra-codemode` tool integration, and MCP client over Streamable HTTP & stdio).
- `pikra-ffi`: Clean C-ABI library for Swift (`PikraKit`) and other native SDKs without bundling `pikra-http`/`rustls`.
- `apps/pikra-rpc`: Headless JSONL-over-stdio RPC binary (`pikra --mode rpc`).

## 2. Testing Philosophy (Strict Red Lines)

- **NO Unit Tests**: NEVER add `#[cfg(test)]` modules inside `src/` files or fragile internal unit tests.
- **Minimal Business E2E Tests Only**: Place end-to-end integration tests under `<crate>/tests/*.rs`. Cover 1-3 core business flows per feature using `pikra-testkit` (deterministic Faux Streaming Provider) or real local pipes/sandboxes. Test code must stay concise (< 50% of production code).
- **Zero Sleep / Timing Flakiness**: NEVER use `sleep` polling or hardcoded wall-clock thresholds in tests; use channels, barriers, or deterministic virtual clocks.

## 3. Production Pitfalls & Prompt Cache Invariants (Must Enforce)

1. **Prompt Cache Prefix Stability**:
   - Base instructions and resident tool definitions (including exact property/tool ordering via `serde_json/preserve_order`) stay byte-identical across turns.
   - Named context updates (`set_context(name, text)`) deduplicate identical values and append `<system-reminder>` blocks between turns rather than mutating leading system instructions.
   - Compaction summary generation reuses the exact parent transcript prefix + a trailing summarization user prompt so it hits prompt cache reads.
2. **Compaction Token Math**:
   - Exclude raw base64 image payload bytes when estimating transcript tokens (count each image at a bounded constant like `1_200` tokens, never `base64.len() / 4`).
   - Cap output reserve tokens during auto-compaction threshold calculation (`min(max_output_tokens, 20_000)` or configured `reserve_tokens` such as `33_000`, falling back to `window / 2` on tiny windows) so 128k `max_output_tokens` models don't prematurely compact a 200k window.
   - Fold previous compaction summaries on repeated compactions and cap compacted summary length as a bounded share of the context window.
3. **Wire Protocol Edge Cases**:
   - **Anthropic Messages**: Merge consecutive `toolResult` messages from parallel tool calls into a single `role: "user"` message containing all `tool_result` blocks; split images out of `is_error: true` tool results into trailing image blocks in the same user message; preserve `thinkingSignature` and `redacted` blocks on same-model turns and strip them on cross-model handoffs; normalize `tool_use` IDs to `^[a-zA-Z0-9_-]{1,64}$`.
   - **OpenAI Responses**: Accept completed message phases (`commentary`, `final_answer`), preserve `item_id` / reasoning encrypted payloads on same-provider turns, clamp `prompt_cache_key` to 64 chars, and treat non-terminal stream notices gracefully.
   - **Pure Assistant Text**: NEVER append operational notices (e.g., length-limit warnings) into assistant `text` blocks; surface truncation strictly through `StopReason::Length` and diagnostics.
4. **Lifecycle & Concurrency**:
   - `abort()` / `interrupt()` must await in-flight tool callback teardown and settle unread `steer` items (emitting `steered` only for consumed ones and dropping unread ones) before emitting `turn_end` / `agent_end`, so immediate subsequent calls never spuriously fail with `busy`.
   - Deferred tools (`tool_search` / `call_tool`): never duplicate schemas across repeated searches, keep deferred tools out of top-level resident `tools`, and allow direct `call_tool` execution for any registered deferred tool even if the model skipped searching first.
   - Swift `URLSession` transport must use `.ephemeral` / `reloadIgnoringLocalCacheData` with `urlCache = nil` so API keys/headers never touch disk caches.

## 4. Git Hygiene

- Never commit `.env`, credentials, or build artifacts (`target/`, `.build/`, `*.xcframework`).
- Stage explicit paths only (`git add <paths>`); never `git add .` or `git add -A`.
- Local commits only; never `git push` unless explicitly instructed by the user.
