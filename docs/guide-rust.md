# Pikra Rust 接入指南

本文说明如何在 Rust 项目（如桌面守护进程、服务端、CLI 或多 Agent 宿主如 Meerkat）中按需嵌入 Pikra。

---

## 1. 按场景选对接入层

Pikra 各 Crate 严格解耦，不要引入不需要的上层包：

| 你的使用场景 | 推荐引入的包 | 说明 |
| :--- | :--- | :--- |
| **只需要统一调各家大模型流式 API** | `pikra-ai` + `pikra-http` | 统一 OpenAI Responses / Codex / Completions / Anthropic / Gemini 流式事件与计费，无会话状态 |
| **自己管存储与多 Agent 调度（如 Meerkat）** | `pikra-kit` + `pikra-tools` + `pikra-http` | **最常用推荐组合**：开箱具备会话树、自动压缩、可扩展 Skills、BM25 延迟工具网关、MCP 与内置工作区工具 |
| **完全自建上下文管线，只要纯内存回合循环** | `pikra-agent` + `pikra-ai` | 纯内存状态机 + 并发工具执行 + `steer` 插话 + Middleware 切面 |
| **没有外部调度器，单进程需防崩溃重复执行工具** | 在上面基础上加 `pikra-durable` | 内嵌 SQLite WAL 事务级 Checkpoint、会话租约隔离与工具幂等恢复 |

---

## 2. 最小可运行示例（`KitSession` + 内置工具箱）

在 `Cargo.toml` 中按需开启所需工具特性（未开启的特性连同其依赖不会参与编译）：

```toml
[dependencies]
pikra-ai = { path = "../pikra/crates/pikra-ai" }
pikra-http = { path = "../pikra/crates/pikra-http" }
pikra-kit = { path = "../pikra/crates/pikra-kit" }
pikra-tools = { path = "../pikra/crates/pikra-tools", default-features = false, features = ["fs", "patch", "search", "shell", "monitor", "sleep", "web-fetch"] }
```

创建带持久化、内置工具与流式输出的会话：

```rust
use futures_util::StreamExt;
use pikra_ai::{ApiKind, Client, Model, ThinkingLevel};
use pikra_http::ReqwestTransport;
use pikra_kit::{KitEvent, KitSession, KitSessionConfig, KitToolSpec};
use pikra_session::JsonlSessionStore;
use pikra_tools::{BuiltinToolKind, BuiltinToolSet, WorkspaceTools, WorkspaceToolsConfig};
use std::sync::Arc;

pub async fn run_example(workspace: &std::path::Path, api_key: String) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 初始化网络传输（Linux/Windows/macOS 自动加载系统根证书）
    let transport = Arc::new(ReqwestTransport::new()?);
    let provider = Arc::new(Client::new(transport.clone()));

    // 2. 选择并构建内置工作区工具
    let tools_cfg = WorkspaceToolsConfig::new(workspace)
        .with_enabled_tools([
            BuiltinToolKind::Read,
            BuiltinToolKind::Write,
            BuiltinToolKind::Edit,
            BuiltinToolKind::ApplyPatch,
            BuiltinToolKind::Find,
            BuiltinToolKind::Grep,
            BuiltinToolKind::ExecCommand,
        ]);
    let builtin_tools = WorkspaceTools::build(tools_cfg);

    // 3. 配置模型与会话（JSONL 追加式落盘）
    let model = Model::new("deepseek-flash", "deepseek", ApiKind::OpenaiCompletions, "https://api.deepseek.com/v1");
    let store = Arc::new(JsonlSessionStore::open(workspace.join(".pikra/session.jsonl"))?);

    let mut cfg = KitSessionConfig::new(model)
        .with_system_prompt("You are a concise engineering assistant.")
        .with_store(store)
        .with_codemode(true);
    cfg.thinking_level = ThinkingLevel::Medium;
    cfg.api_key = Some(api_key);
    for tool in builtin_tools {
        cfg = cfg.with_resident_tool(tool);
    }

    let session = KitSession::new(provider, cfg)?;

    // 4. 发起对话并消费实时事件流
    let mut stream = session.prompt_stream("查找项目里的所有 Cargo.toml 并总结工作区结构")?;
    while let Some(event) = stream.next().await {
        if let KitEvent::Agent(agent_ev) = event {
            println!("{agent_ev:?}");
        }
    }
    Ok(())
}
```

---

## 3. 定制内置工具（改名 / 改描述 / 裁剪参数）与注册自定义工具

### 3.1 覆盖内置工具的描述或隐藏参数
做具体产品时，往往需要针对产品场景改写内置工具提示词、改工具名，或屏蔽某些可选参数并注入固定默认值：

```rust
use pikra_tools::{BuiltinToolKind, DescriptionOverride, ToolDefinitionOverride, WorkspaceToolsConfig};
use serde_json::{Map, json};

let tools_cfg = WorkspaceToolsConfig::new(workspace).with_tool_override(
    BuiltinToolKind::Grep,
    ToolDefinitionOverride {
        rename: Some("search_code".into()),
        description: Some(DescriptionOverride::Append(
            "Prefer fixed_strings=true for exact symbol lookups.".into(),
        )),
        hidden_parameters: vec!["context_lines".into()],
        default_arguments: Map::from_iter([("context_lines".into(), json!(2))]),
        ..Default::default()
    },
);
```

### 3.2 用闭包快速编写宿主自定义工具
```rust
use pikra_agent::{AgentToolResult, ClosureTool};
use pikra_schema::{obj_schema, str_schema, with_description};

let custom_tool = Arc::new(ClosureTool::parallel(
    "get_ticket",
    "Fetch issue details by ticket ID.",
    obj_schema([("id", with_description(str_schema(), "Ticket ID"), true)]),
    |_call_id, args, _cancel, _on_update| Box::pin(async move {
        let id = args["id"].as_str().ok_or("missing id")?;
        Ok(AgentToolResult::text(format!("Ticket {id}: status=open")))
    }),
));
```

---

## 4. 前后台交互终端、`Monitor` 与后台事件唤醒

当启用 `exec_command` / `write_stdin`、`monitor` 和 `sleep` 时，推荐将 `pikra-tools` 的后台事件回调接到 `KitSession` 的后台唤醒收件箱：

1. **短命令与长驻进程合一（`exec_command` + `write_stdin`）**：
   - 命令在 `yield_time_ms`（默认 10s）内退出时直接返回完整结果；
   - 超时未结束（如 `pnpm dev` 或长构建）自动转入会话后台池并返回 `session_id`，模型可用 `write_stdin` 读取增量输出、向 PTY 输入交互指令或传 `kill: true` 结束整个进程组。
   - `ProcessSessionManager` 与 `MonitorManager` 在 Drop 或调 `shutdown()` 时自动向所有活跃负进程组发送 `SIGKILL`，确保零孤儿进程残留。
2. **事件驱动监听（`monitor`）与自动唤醒**：
   - `monitor` 在后台监听进程 `stdout`（200ms 窗口合并多行，1 分钟超 200 行自动熔断防刷屏，完整输出落盘 `.log`）。
   - 事件推入 `KitSession` 收件箱后：若当前正在执行 `sleep`，立即提前唤醒；若会话空闲，订阅 `subscribe_background_wakeups()` 的宿主直接调用 `session.run_background_turn()` 将合并后的所有事件一次性开一轮后台回复；若正在回复用户消息，则自动排队等当前轮结束。

---

## 5. 可配置 `AGENTS.md` 规则发现、Skills 与动态工具加载

### 5.1 配置多层级 `AGENTS.md` 规则与 Skill 目录策略
Pikra 作为无头引擎**绝不在库层写死任何特定品牌目录（如 `.pikra`）**：默认只识别跨生态中立目录（`.agents/skills`、`~/.agents`），所有回溯边界、候选文件名、自定义目录与字节上限均支持「默认值优先 + 全项覆盖」：

```rust
use pikra_kit::{
    InstructionDiscoveryConfig, SkillDiscoveryConfig, SkillEnableRule, SkillSelector, WalkBoundary,
};

// 1. AGENTS.md 规则加载（默认停在 .git 根、32 KiB 封顶、完全相同正文自动去重防重复）
let instruction_cfg = InstructionDiscoveryConfig {
    cwd: Some(workspace.to_path_buf()),
    walk_boundary: WalkBoundary::ProjectRoot,   // 可选 ProjectRoot | FilesystemRoot | CurrentDirOnly
    user_dirs: vec!["~/.my-app".into()],        // 宿主自定义全局目录（库层默认为空）
    max_bytes: Some(32 * 1024),                 // 传 None 则不限制字节上限
    deduplicate_identical_content: true,        // 多份内容完全一致的 AGENTS.md 自动跳过不重复加载
    ..Default::default()
};

// 2. Skills 目录发现（共用同一套 WalkBoundary）
let skill_cfg = SkillDiscoveryConfig {
    cwd: Some(workspace.to_path_buf()),
    walk_boundary: WalkBoundary::ProjectRoot,
    project_skill_dirs: vec![".my-app/skills".into(), ".pi/skills".into()],
    include_agents_skills: true,                // 扫描项目与全局 .agents/skills
    include_user_skills: true,                  // 测试隔离时设为 false
    rules: vec![SkillEnableRule {
        selector: SkillSelector::ByName("legacy-helper".into()),
        enabled: false,
    }],
    ..Default::default()
};
```

- **保前缀缓存 + 压缩防遗忘**：`AGENTS.md` 通过具名上下文块注入，首轮进入缓存前缀，触发自动压缩（Compaction）后原样保留不会丢失；`Skills` 中途热更新调用 `session.reload_skills()` 只在末尾追加 `# Skills changed` 增删增量，修改正文不刷屏。

### 5.2 延迟加载海量工具与 MCP
- 固定高频工具用 `KitToolSpec::resident(tool)`；
- 低频或 MCP 工具用 `KitToolSpec::deferred(tool)`（或设置 `auto_defer_threshold_bytes` 超阈值自动转延迟）；
- 模型通过 `tool_search`（BM25 分词打分）按需查 Schema、用 `call_tool` 执行（返回结果的 `details.delegated_tool` 原生携带真实工具名与参数，UI 可直接渲染真实工具卡片），顶层 `tools` 列表整场会话一字不变。

### 5.3 注入仅模型可见的隐藏上下文（`is_meta`）
当宿主需要追加系统提醒、外部事件唤醒或运行中协议纠偏（模型可见、聊天 UI 不可见）时，按注入时机直接调用对应接口，无需手拼或正则过滤：

```rust
// 1. 轮前排队伴随注入（随下一次用户提问一起发出）
session.set_context("git_status", "branch: main");      // 同名同值自动去重（保缓存）
session.append_context("Detected external file edit."); // 一次性 <system-reminder>
session.append_meta("<custom-hint>...</custom-hint>");  // 一次性无额外包装的隐藏 UserMessage

// 2. 无用户发言，直接用隐藏消息触发一轮推理（如定时心跳、外部事件触发）
let _ = session.prompt_meta("Check pending background notifications.").await?;

// 3. 运行中动态注入隐藏纠偏 / 提醒（不触发 UI 的 Steered 用户气泡事件）
session.steer_meta("protocol-fix", "<system-reminder>Finish tool calls before replying.</system-reminder>");
session.follow_up_meta("post-check", "Verify formatting before finishing.");

// 4. UI 渲染过滤：messages() 保留完整消息（含 user.is_meta == true），visible_messages() 仅返回用户可见对话
let chat_history = session.visible_messages();
```
- `pikra-durable` 同样提供事务级 `engine.set_context(...)`、`engine.append_meta(...)` 与 `engine.submit_meta(...)`。

---

## 6. 拦截切面（权限审批与输出密钥脱敏）

不要为了加审批或打码去重写工具实现，直接挂载 `AgentMiddleware`：
- `before_tool_call`：拦截敏感工具（如 `exec_command` / `write` / `apply_patch`）弹出用户确认，拒绝时设 `block: true` 返回原因，也可通过 `args_override` 改写参数。
- `after_tool_call`：统一扫描工具输出文本，对注入过的 API Key / 凭证做打码脱敏，或将超长输出落盘截断。
