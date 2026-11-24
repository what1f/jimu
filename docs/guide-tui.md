# Rust 终端界面（`pikra-tui`）

`pikra-tui` 是一个**框架中立、按需勾选能力编译**的桌面终端 UI crate，底层原版迁移自 Grok Build 的终端渲染引擎（全 6 套明暗主题 + `Auto` 自适应、流式 Markdown 表格/代码块/LaTeX、多行原子元素输入器、VTE 与 Diff 原语），上层抽象出极薄的 `TuiHarness` 契约——**默认零 `pikra-*` 依赖**，既可直接接入任意自定义 Agent Harness，也可通过 `features = ["pikra"]` 原生直连 `KitSession`（零 ACP 限制）。

---

## 1. 按需勾选 Feature 编译

```toml
# 场景 A：接入自己的 Agent Harness（零 pikra-* 依赖）
pikra-tui = { path = "crates/pikra-tui" }

# 场景 B：原生直连 Pikra（KitSession + 审批 + 上下文进度条 + 树形回溯 + 后台进程）
pikra-tui = { path = "crates/pikra-tui", features = ["pikra"] }

# 场景 C：按需额外开启剪贴板贴图 / 终端内联图片 / Mermaid 接口
pikra-tui = { path = "crates/pikra-tui", features = ["pikra", "clipboard-image", "inline-image", "mermaid"] }
```

| Feature | 默认 | 说明 |
| :--- | :---: | :--- |
| `syntax` | ✓ | 纯 Rust `syntect`（`default-fancy`）+ `two-face` 语法高亮（含预构建 Swift 语法） |
| `system-appearance` | ✓ | OSC11 + 系统暗/亮外观监听（自动在 `Light` 浅色与 `Dark` 深色间切换） |
| `history-search` | ✓ | `Ctrl+R` 历史输入模糊搜索 |
| `scrolling-regions` | ✓ | DECSTBM 滚动区域优化 |
| `pikra` | — | 开启 `pikra_tui::pikra` 原生适配器（直连 `KitSession` / `AgentMiddleware` / `ProcessSessionManager` / `MonitorManager`） |
| `clipboard-image` | — | 剪贴板图片校验、缩放与 MIME/Base64 辅助 |
| `inline-image` | — | Kitty / iTerm2 终端内联图片渲染 |
| `mermaid` | — | 可插拔 `MermaidEngine` trait 与超时隔离封装 |

---

## 2. 原生接入 Pikra（`features = ["pikra"]`）

```rust
use pikra_ai::StreamProvider;
use pikra_kit::{KitSession, KitSessionConfig};
use pikra_tui::{
    app::{TuiApp, TuiAppConfig},
    engine::theme::ThemeKind,
    pikra::{PikraApprovalMiddleware, PikraHarness, PikraInteractionSink},
};
use std::sync::Arc;

async fn run_pikra_tui(
    provider: Arc<dyn StreamProvider>,
    mut kit: KitSessionConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let sink = PikraInteractionSink::new();
    let approval = PikraApprovalMiddleware::new(sink.clone(), |ctx| {
        matches!(ctx.tool_call.name.as_str(), "write" | "edit" | "apply_patch")
    });
    kit.middlewares.push(Arc::new(approval));

    let session = Arc::new(KitSession::new(provider, kit)?);
    let harness = Arc::new(PikraHarness::with_interaction_sink(session, sink));

    let config = TuiAppConfig {
        theme_kind: ThemeKind::Auto, // 自动跟随终端/系统明暗（Light / Dark）
        ..Default::default()
    };
    TuiApp::new(harness, config).run().await?;
    Ok(())
}
```

`PikraHarness` 可按需挂接更多能力：
- `.with_models(models)`：解锁 `/model` 与 `/effort`（`/thinking`）模型与思考档位切换；
- `.with_permission_mode(controller)`：解锁 `/permissions` 与 `Shift+Tab` 权限档位循环；
- `.with_session_controller(controller)`：解锁 `/resume` 恢复历史会话与 `/new`（`/clear`）新建会话；
- `.with_process_manager(pm)` / `.with_monitor_manager(mm)`：当有后台 `exec_command` 或 `monitor` 运行时，自动在底部状态条展示，点击可查看与终止（空闲无后台进程时自动隐藏）；
- 默认已自动接通：`ContextMeterCap`（状态栏上下文进度条与 `/context`）、`RewindCap`（原生树形分支 `/rewind` / `/tree` 与双击 `Esc`）、`CompactionCap`（`/compact`）、`SkillsCap`（`/skills`，随回合与查询自动热加载）、后台事件自动唤醒。

---

## 3. 接入自定义 Agent Harness（零 `pikra-*` 依赖）

任何 Rust Agent 运行时只需实现 `pikra_tui::harness::TuiHarness` 的 3 个核心方法（`submit` / `subscribe` / `abort`）即可复用全部终端 UI 能力；高阶能力通过 `capabilities()` 返回 `Option<&dyn ...>` 按需点亮（未实现的自动隐藏）：

- `ContextMeterCap`：上下文进度条与 `/context`
- `ModelPickerCap`：模型与思考档位切换（`/model`、`/effort` / `/thinking`、`Alt+P`、`Alt+T`）
- `PermissionModeCap`：权限模式切换（`/permissions`、`Shift+Tab`）
- `SessionPickerCap`：会话恢复与新建（`/resume`、`/new` / `/clear`）
- `RewindCap`：树形会话回溯与分支切换（`/rewind` / `/tree`、空框双击 `Esc`；选中 user 节点回退并回填输入框编辑重发，选中 assistant 节点直接切换活跃叶节点）
- `TasksCap`：底部活跃后台进程提示条与点击终止
- `SkillsCap` / `McpCap` / `CompactionCap` / `QueueCap`

交互弹窗通过发送 `TuiEvent::Interaction(InteractionRequest::...)` 或调用 `app.controller()` 发起，内置支持：
- `ApproveTool`（工具审批：Allow / Always allow / Deny）
- `AskQuestion`（单选 / 多选 / 自由输入提问卡）
- `ApprovePlan`（带 Markdown 预览的审批卡）
- `Select`（通用可过滤选择弹窗）

---

## 4. 主题、快捷键与 Slash Commands

- **6 套内置原版主题 + `Auto`**：`dark`（默认深色）、`light`（浅色）、`tokyonight`、`rosepine-moon`、`oscura-midnight`、`terminal`（透明跟随终端底色）、`auto`（自动探测 OSC11 / `COLORFGBG` / 系统暗亮并监听切换）。使用 `/theme <name>` 或 `/theme` 弹窗切换。
- **常用快捷键**：
  - `Esc`：中止当前生成 / 关闭浮层 / 空输入框**双击 `Esc`** 打开 Rewind / Tree 面板
  - `Ctrl+C`：中止当前生成 / 清空草稿 / 空输入框**连按两次退出**
  - `Ctrl+D`（空框）/ `Ctrl+Q`（随时）：退出
  - `Shift+Tab`：循环切换权限模式
  - `Alt+T`：循环切换思考强度；`Alt+P`：打开模型选择器
  - `Ctrl+O`：展开/折叠全部工具与思考块；`Ctrl+R`：输入历史搜索
  - `Enter` 发送/插话、`Shift+Enter` / `Ctrl+J` 换行、`Alt+Enter` 排队 `follow_up`、`Tab` 补全
- **内置 Slash Commands**：
  - 会话：`/new`（别名 `/clear`，新建会话）、`/resume`、`/rewind`（别名 `/tree`）、`/export [path]`、`/exit`（别名 `/quit`）
  - 模型与权限：`/model`、`/effort`（别名 `/thinking`）、`/compact`、`/context`、`/permissions`
  - 扩展与界面：`/skills`、`/mcp`、`/copy`、`/theme`、`/help`（别名 `/hotkeys`）
- **工具卡片与插槽**：通过 `config.tool_renderers.register(name, ...)` 覆盖或扩展任意工具卡片；通过 `config.chrome.slots`（`ChromeSlots`）定制 `welcome` / `header` / `footer` / `status_line` / 动态 `status_flags`；通过 `config.slash_commands` 注入或覆盖内置命令。
- **终端程序状态**：默认发送 OSC 7501，覆盖空闲、工作中、等待权限或回答、完成、错误和退出清理。使用 `ChromeConfig::app_name` 配置程序标识，通过 `osc7501_enabled` 控制是否发送。

---

## 5. 直接通过统一 CLI 启动（`pikra tui`）

`apps/pikra` 默认启用可选 `tui` feature（无头部署可用 `--no-default-features` 零 `pikra-tui` 依赖构建），并与 `pikra acp` 共享同一套认证（API Key / `pikra login` Codex OAuth / `~/.pikra/models.json`）、全量 11 个内置工具（含 `--tools` / `--disable-tools` / `--no-tools`）、Codemode、JS 扩展、Skills、`AGENTS.md` 发现、默认系统提示词以及 `~/.pikra/sessions/` 会话持久化：

```bash
# 直接以 DeepSeek Flash + 高思考强度启动（自动读取指定环境变量）
cargo run --release -p pikra -- tui \
  --provider deepseek --model deepseek-flash --api-key-env TEST_DEEPSEEK_API_KEY \
  --thinking high --permission-mode default

# 无头构建（零 pikra-tui 依赖）
cargo build --release -p pikra --no-default-features
```

TUI 权限模式保留 3 档（`default` / `acceptEdits` / `bypassPermissions`，可用 `Shift+Tab` 或 `/permissions` 实时切换），同时支持 `/fast [on|off]`、`/effort`、`/model`、`/resume`、`/new`、`/rewind`（双击 `Esc`）、`/copy`、`/export`、`/theme` 等全部命令。
