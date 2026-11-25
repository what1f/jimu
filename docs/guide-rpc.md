# Jimu JSONL-over-stdio RPC 接入指南

对于 Node.js、Python、Go、Tauri 外部宿主或跨进程隔离场景，可以直接启动 `jimu rpc`（等价于 `jimu --mode rpc`）子进程，通过标准输入输出（严格 `\n` LF 分隔的 UTF-8 JSON 行）驱动完整 Jimu 会话。

---

## 1. 启动参数

```bash
jimu --mode rpc \
  --cwd /path/to/workspace \
  --session /path/to/session.jsonl \
  --provider anthropic \
  --model claude-sonnet-5-5 \
  --skill-dir /path/to/extra-skills
```

- **输入（`stdin`）**：每行一条 JSON 命令对象（可选携带 `"id": "req-1"` 用于请求-响应关联）。
- **输出（`stdout`）**：每行一条 JSON 消息，分为两类：
  - `"type": "response"`：对应某条输入命令的同步确认或查询结果（原样回传 `"id"`、`"command"`、`"success"`、`"data"` / `"error"`）；
  - 流式会话事件：回合执行期间实时推送的 `agent_start`、`message_update`（含 `text_delta` / `thinking_delta` / `toolcall_delta`）、`tool_execution_start`、`tool_execution_end`、`agent_end`、`compaction_start` / `compaction_end` 等事件。
- **单次命令行模式与全局配置**：若只需跑一轮非交互任务，可直接用 `jimu -p "你的问题"`（或配合 `--mode json` 输出 JSONL 事件流；失败或报错时向 `stderr` 输出原因并以非 0 状态码退出）。`jimu -p`、`--mode text|json` 与 `jimu rpc` 统一读取全局 `~/.jimu/providers.json`（`base_url`、`api`、`api_key_env`、`models`）与 `~/.jimu/auth.json`，优先级为 `CLI 参数 > providers.json > 内置接入预设`。
- **默认系统提示词、细粒度工具裁剪与统一 MCP / 延迟网关**：未传 `--system-prompt` 时默认注入简洁通用的系统提示词与 `<environment>`（工作目录与 OS）；支持通过 `--tools <csv>`（白名单）、`--disable-tools <csv>`（黑名单）或 `--no-tools` 精细控制启用的内置工具（可选名：`read`、`write`、`edit`、`apply_patch`、`find`、`grep`、`exec_command`、`write_stdin`、`monitor`、`sleep`、`web_fetch`）；与 TUI / ACP 共享同一套 `KitSession`、延迟工具网关（`tool_search` / `call_tool`，仅纯 `--no-tools` 且无扩展/MCP/Codemode 时关闭）及 MCP 配置面（读取 `~/.jimu/settings.json` 的 `mcp_servers`，并支持 `--mcp-config <path>` 与 `--mcp-server <name>=<url|command|json>`）。
- **Skills 自动热加载**：每轮 `prompt` 与 `jimu -p` 前自动重扫 Skills 目录（支持软链以及 `$skill` / `/skill` 显式引用），中途增删、移动或软链重定向时自动追加 `# Skills changed` 增量而不破坏前缀缓存。
- **网页访问**：RPC 会话中的 `web_fetch` 逐跳检查目标。符合公网域名规则的名称可解析到 `198.18.0.0/15` TUN Fake-IP。工具拦截该网段的 IP 字面量、本地地址和链路本地地址。

---

## 2. 核心命令速查

### 对话与流中控制
```json
{"id":"1","type":"prompt","message":"重构 src/lib.rs 并跑通测试"}
{"id":"2","type":"steer","message":"先别改公开函数签名"}
{"id":"3","type":"follow_up","message":"改完顺便检查一下 clippy"}
{"id":"4","type":"abort"}
```
- 当回合正在进行时，发送 `"type": "prompt"` 也可携带 `"streamingBehavior": "steer"` 或 `"followUp"`。
- 支持多模态图片输入：在 `prompt` / `steer` / `follow_up` 中附带 `"images": [{"data": "<base64>", "mimeType": "image/png"}]`。

### 模型、思考档位与动态隐藏上下文（`isMeta`）
```json
{"id":"5","type":"set_model","provider":"openai","modelId":"gpt-5.4","api":"openai-responses"}
{"id":"6","type":"set_thinking_level","level":"high"}
{"id":"7","type":"set_fast","enabled":true}
{"id":"8","type":"set_context","name":"git_status","text":"branch: main (clean)"}
{"id":"9","type":"append_context","name":"file_watch","text":"Cargo.toml changed externally"}
{"id":"10","type":"append_meta","text":"<system-reminder>Custom unwrapped reminder</system-reminder>"}
{"id":"11","type":"prompt","message":"Run background check","isMeta":true}
{"id":"12","type":"steer","message":"Complete pending verification first","isMeta":true}
{"id":"13","type":"get_messages","includeMeta":false}
```
- `set_model` 切换的模型会自动持久化进会话元数据（`jimu.rpc.model`），进程重启恢复同一会话文件时自动还原。
- `set_context` 同名同值自动去重，仅在内容变化时于下一轮前作为 `<system-reminder>` 追加，不破坏已有前缀缓存。
- `prompt` / `steer` / `follow_up` 携带 `"isMeta": true`（或使用 `append_context` / `append_meta`）时，消息对模型正常可见但标记 `"is_meta": true`（隐藏插话不发 `steered` 事件）；调用 `get_messages` 传 `"includeMeta": false` 可直接获取过滤掉系统注入后的纯对话列表。

### 会话树、分叉与上下文压缩
```json
{"id":"9","type":"compact","customInstructions":"保留所有文件路径和未解决报错"}
{"id":"10","type":"get_tree"}
{"id":"11","type":"fork","entryId":"entry_01..."}
{"id":"12","type":"switch_session","sessionPath":"/path/to/another.jsonl"}
{"id":"13","type":"reload_extensions"}
```
- `compact` 为异步可取消任务：执行期间仍可发送 `{"type":"abort"}` 立即中断压缩，不会阻塞 RPC 控制循环。
