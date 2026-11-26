# Rust 终端界面（`jimu-tui`）

`jimu-tui` 是一个**框架中立、按需勾选能力编译**的桌面终端 UI crate，底层原版迁移自 Grok Build 的终端渲染引擎（全 6 套明暗主题 + `Auto` 自适应、流式 Markdown 表格/代码块/LaTeX、多行原子元素输入器、VTE 与 Diff 原语），上层抽象出极薄的 `TuiHarness` 契约——**默认零 `jimu-*` 依赖**，既可直接接入任意自定义 Agent Harness，也可通过 `features = ["jimu"]` 原生直连 `KitSession`（零 ACP 限制）。

---

## 1. 按需勾选 Feature 编译

```toml
# 场景 A：接入自己的 Agent Harness（零 jimu-* 依赖）
jimu-tui = { path = "crates/jimu-tui" }

# 场景 B：原生直连 Jimu（KitSession + 审批 + 上下文进度条 + 树形回溯 + 后台进程）
jimu-tui = { path = "crates/jimu-tui", features = ["jimu"] }

# 场景 C：按需额外开启剪贴板贴图 / 终端内联图片 / Mermaid 接口
jimu-tui = { path = "crates/jimu-tui", features = ["jimu", "clipboard-image", "inline-image", "mermaid"] }
```

| Feature | 默认 | 说明 |
| :--- | :---: | :--- |
| `syntax` | ✓ | 纯 Rust `syntect`（`default-fancy`）+ `two-face` 语法高亮（含预构建 Swift 语法） |
| `system-appearance` | ✓ | OSC11 + 系统暗/亮外观监听（自动在 `Light` 浅色与 `Dark` 深色间切换） |
| `history-search` | ✓ | `Ctrl+R` 历史输入模糊搜索 |
| `scrolling-regions` | ✓ | DECSTBM 滚动区域优化 |
| `jimu` | — | 开启 `jimu_tui::jimu` 原生适配器（直连 `KitSession` / `AgentMiddleware` / `ProcessSessionManager` / `MonitorManager`） |
| `clipboard-image` | — | 剪贴板图片校验、缩放与 MIME/Base64 辅助 |
| `inline-image` | — | Kitty / iTerm2 终端内联图片渲染 |
| `mermaid` | — | 可插拔 `MermaidEngine` trait 与超时隔离封装 |

---

## 2. 原生接入 Jimu（`features = ["jimu"]`）

```rust
use jimu_ai::StreamProvider;
use jimu_kit::{KitSession, KitSessionConfig};
use jimu_tui::{
    app::{TuiApp, TuiAppConfig},
    engine::theme::ThemeKind,
    jimu::{JimuApprovalMiddleware, JimuHarness, JimuInteractionSink},
};
use std::sync::Arc;

async fn run_jimu_tui(
    provider: Arc<dyn StreamProvider>,
    mut kit: KitSessionConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let sink = JimuInteractionSink::new();
    let approval = JimuApprovalMiddleware::new(sink.clone(), |ctx| {
        matches!(ctx.tool_call.name.as_str(), "write" | "edit" | "apply_patch")
    });
    kit.middlewares.push(Arc::new(approval));

    let session = Arc::new(KitSession::new(provider, kit)?);
    let harness = Arc::new(JimuHarness::with_interaction_sink(session, sink));

    let config = TuiAppConfig {
        theme_kind: ThemeKind::Auto, // 自动跟随终端/系统明暗（Light / Dark）
        ..Default::default()
    };
    TuiApp::new(harness, config).run().await?;
    Ok(())
}
```

`JimuHarness` 可按需挂接更多能力：
- `.with_models(models)`：解锁 `/model` 与 `/effort`（`/thinking`）模型与思考档位切换；
- `.with_permission_mode(controller)`：解锁 `/permissions` 与 `Shift+Tab` 权限档位循环；
- `.with_mcp(mcp_cap)`：解锁 `/mcp` 查看已连接 MCP 服务器及暴露工具数；
- `.with_session_controller(controller)`：解锁 `/resume` 恢复历史会话与 `/new`（`/clear`）新建会话；
- `.with_shell_cwd(cwd)`：开启 `!` 本地快捷 Shell 模式并绑定工作目录（`ShellCap`），独立进程组运行并支持 `Ctrl+C`/`Esc` 随时取消，结果渲染到对话区并追加至会话上下文；
- `.with_process_manager(pm)` / `.with_monitor_manager(mm)`：当有后台 `exec_command` 或 `monitor` 运行时，自动在底部状态条展示，点击可查看与终止（空闲无后台进程时自动隐藏）；
- 默认已自动接通：`ContextMeterCap`（状态栏上下文进度条与 `/context`）、`RewindCap`（原生树形分支 `/rewind` / `/tree` 与双击 `Esc`）、`CompactionCap`（`/compact`）、`SkillsCap`（`/skills`，随回合与查询自动热加载）、后台事件自动唤醒。

---

## 3. 接入自定义 Agent Harness（零 `jimu-*` 依赖）

任何 Rust Agent 运行时只需实现 `jimu_tui::harness::TuiHarness` 的 3 个核心方法（`submit` / `subscribe` / `abort`）即可复用全部终端 UI 能力；高阶能力通过 `capabilities()` 返回 `Option<&dyn ...>` 按需点亮（未实现的自动隐藏）：

- `ContextMeterCap`：上下文进度条与 `/context`
- `ModelPickerCap`：模型与思考档位切换（`/model`、`/effort` / `/thinking`、`Alt+P`、`Alt+T`）
- `PermissionModeCap`：权限模式切换（`/permissions`、`Shift+Tab`）
- `SessionPickerCap`：会话恢复与新建（`/resume`、`/new` / `/clear`）
- `RewindCap`：紧凑树形会话回溯与分支切换（`/rewind` / `/tree`、空框双击 `Esc`；单链保持平铺不右漂，分叉处以 `├─`/`└─` 与竖线对齐，活跃分支排前且打开时默认聚焦当前叶子 `●`；选中 user 节点回退并回填输入框编辑重发，选中 assistant 节点直接切换活跃叶节点）
- `TasksCap`：底部活跃后台进程提示条与点击终止
- `ShellCap`（`HarnessCapabilities::shell`）：空输入按 `!` 进入本地快捷 Shell 模式（独立进程组执行，支持 `Ctrl+C`/`Esc` 取消，结果渲染到对话区并追加至会话上下文）
- `SkillsCap` / `McpCap` / `CompactionCap` / `QueueCap`

交互弹窗通过发送 `TuiEvent::Interaction(InteractionRequest::...)` 或调用 `app.controller()` 发起，内置支持：
- `ApproveTool`（工具审批：Allow / Always allow / Deny）
- `AskQuestion`（单选 / 多选 / 自由输入提问卡）
- `ApprovePlan`（带 Markdown 预览的审批卡）
- `Select`（通用可过滤选择弹窗）

---

## 4. 主题、图片粘贴、快捷键与扩展插槽

- **6 套内置原版主题 + `Auto` + 外部 JSON 自定义主题**：`dark`（默认深色）、`light`（浅色）、`tokyonight`、`rosepine-moon`、`oscura-midnight`、`terminal`（透明跟随终端底色）、`auto`（自动探测 OSC11 / `COLORFGBG` / 系统暗亮并监听切换）。通过 `TuiAppConfig.custom_themes` / `initial_custom_theme` / `theme_overrides`（`ThemeOverrides { base, colors }`）或 `TuiController::register_custom_theme` / `set_theme_overrides` 注入外部自定义主题与增量覆盖；使用 `/theme <name>` 或 `/theme` 弹窗切换，`on_theme_name_change` 同步持久化内置与自定义主题名。
- **图片粘贴与 `[Image #N]` 芯片**：按 `Ctrl+V`（终端拦截时可用 `Alt+V`）读取剪贴板图片，或在输入框粘贴/拖入本地图片路径，均自动转为原子 `[Image #N]` 芯片并在会话记录中保留 `[Image #N]` 占位标识。
- **极简默认布局（默认隐藏顶栏 + 底部默认状态栏）**：`ChromeConfig.show_header` 默认为 `false`，将顶部 1 行空间留给对话区；底部默认状态栏展示当前模型、Effort、`fast` 标记、工作目录、Git 分支、上下文占用、权限模式与运行中 Spinner。
- **多行输入、外置编辑器与历史持久化**：
  - **多行输入与外置编辑器**：`Shift+Enter` / `Alt+Enter`（`Option+Enter`）/ `Ctrl+J` / `\`+`Enter` 换行；`Ctrl+Enter` 排队 `follow_up`；`Alt+M`（Kitty 终端下也支持 `Ctrl+M`）或 `/multiline` 切换持久多行模式；`Ctrl+G` 或 `/edit` 调用 `$VISUAL`/`$EDITOR` 编辑长 Prompt（`TuiAppConfig::external_editor` 可自定义覆盖）。
  - **历史回溯与跨会话持久化**：`Up`/`Down` 在首行/末行原地回溯历史并暂存当前草稿；`Ctrl+R` 或 `/history` 打开模糊搜索面板；通过 `TuiAppConfig::prompt_history_path` 与 `prompt_history_limit`（默认 `Some(1000)`）启用跨会话 JSONL 持久化历史（库层零品牌目录硬编码，`apps/jimu` 传入 `~/.jimu/history.jsonl`）。
  - **草稿保护、芯片与快捷交互**：`Ctrl+S` / `Alt+S` 暂存与恢复草稿（发送后自动恢复，刚暂存后按 `Ctrl+Z` 立即取回）；`Ctrl+C` 清空草稿后可按 `Ctrl+Z` 撤销恢复；粘贴/文件/图片芯片支持 `Enter` 交互，`@file` 支持 `Ctrl+L` / `:` 文件预览；空输入按 `Up` 优先聚焦 Queue、按 `Enter` 立即发送队首；补全下拉框支持 `Ctrl+P`/`Ctrl+N`/`Shift+Tab` 导航，选中无参斜杠命令按 `Enter` 一步执行。
  - **快捷 Bash 模式（`!`）**：空输入按 `!` 进入本地 Shell 模式（`HarnessCapabilities::shell` / `ShellCap`，`JimuHarness::with_shell_cwd`），独立进程组运行并支持 `Ctrl+C`/`Esc` 随时取消，结果渲染到对话区并追加至会话上下文。
- **常用快捷键**：
  - `Esc`：中止当前生成 / 关闭浮层 / 取消 `!` Shell 命令 / 空输入框**双击 `Esc`** 打开 Rewind / Tree 面板
  - `Ctrl+C`：中止当前生成 / 取消 `!` Shell 命令 / 清空草稿（可 `Ctrl+Z` 撤销恢复）/ 空输入框**连按两次退出**
  - `Ctrl+D`（空框）/ `Ctrl+Q`（随时）：退出
  - `Ctrl+V` / `Alt+V`：从剪贴板粘贴图片为 `[Image #N]` 芯片
  - `Shift+Tab`：循环切换权限模式（补全下拉打开时用于反向导航）
  - `Alt+T`：循环切换思考强度；`Alt+P`：打开模型选择器
  - `Ctrl+O`：展开/折叠全部工具与思考块；`Ctrl+R`：输入历史搜索；`Ctrl+G`：外置编辑器；`Ctrl+S` / `Alt+S`：暂存/恢复草稿
  - `Enter` 发送/插话、`Shift+Enter` / `Alt+Enter` / `Ctrl+J` / `\`+`Enter` 换行、`Ctrl+Enter` 排队 `follow_up`、`Alt+M` 切换持久多行、`Tab` 补全
- **内置 Slash Commands、Skills 与高亮**：
  - 严格行首 `/` 触发命令与 Skill（保留前导空格；多段/带点绝对路径或已存在的单段路径自动放行作为普通消息；真实拼错给出编辑距离 ≤ 2 的相近命令提示）；粘贴事件主动关闭补全不打断输入。
  - 行首 `/` 下拉同时合并内置/宿主命令与动态发现的 Skills（`/<skill-name>`，重名时支持 `/skill:<name>`），行首触发直接展开 `SKILL.md` 正文（支持 `$ARGUMENTS` / `$1` 参数替换）并在聊天区保持简洁展示；句中空白或标点后支持 `/<skill>` 与 `$<skill>` 补全引用；输入 `/model `、`/effort `、`/permissions `、`/theme `、`/fast ` 及任意实现 `SlashCommand::completions` 的宿主命令均支持参数 Inline 模糊补全；输入框内所有已识别的 `/command`、`/skill` 与 `$skill` 均以主题强调色实时高亮。
  - `/model`（`Alt+P`）打开组合选择器：上方可搜索模型列表（`↑`/`↓`），下方 Effort 进度条（`←`/`→` 或鼠标点选），一次回车同时应用并自动持久化默认模型、思考强度、权限模式、主题与极速模式到 `~/.jimu/settings.json`。
  - 会话：`/new`（别名 `/clear`，新建会话）、`/resume`、`/rewind`（别名 `/tree`）、`/export [path]`、`/exit`（别名 `/quit`）
  - 模型与权限：`/model [provider/id]`、`/effort`（别名 `/thinking`）、`/fast [on|off]`、`/compact`、`/context`、`/permissions`
  - 输入与历史：`/multiline`、`/edit`、`/history`
  - 扩展与界面：`/skills`、`/mcp`、`/reload`（热重载 JS 扩展、UI Mod 与自定义主题）、`/copy`、`/theme`、`/help`（别名 `/hotkeys`）
- **工具卡片与 `ChromeSlots` 装饰钩子**：
  - 通过 `config.tool_renderers.register(name, ...)` 覆盖或扩展任意工具卡片；
  - 通过 `config.chrome.slots`（`ChromeSlots`）同步定制 `welcome` / `header` / `above_prompt` / `footer` / `status_line` / 动态 `status_flags` / `spinner_override`，以及面向消息与工具行的 `decorate_user_message` / `decorate_tool_row`（返回 `SurfaceDecoration { prefix_lines, suffix_lines, left_border, replace }`：留空 `replace` 可在不丢失原生高亮、图片芯片与 Diff 渲染的前提下包裹前后缀与左侧色条，设置 `replace` 则整块替换）；
  - 通过 `config.slash_commands` 注入或覆盖内置命令。
- **终端程序状态**：默认发送 OSC 7501，覆盖空闲、工作中、等待权限或回答、完成、错误和退出清理。使用 `ChromeConfig::app_name` 配置程序标识，通过 `osc7501_enabled` 控制是否发送。

---

## 5. 直接通过统一 CLI 启动（默认 `jimu`）

`apps/jimu` 默认启用可选 `tui` feature（无头部署可用 `--no-default-features` 零 `jimu-tui` 依赖构建），**直接运行 `jimu` 即可进入交互式 TUI**（支持直接带初始消息 `jimu "问题"`、`-c/--continue` 续接当前目录最近会话、`-r/--resume` 打开会话选择器），并与 `jimu acp`、`jimu rpc`、`jimu -p` 通过同一套共享宿主装配层共用全局配置与认证（API Key / `jimu login` Codex OAuth / `~/.jimu/providers.json` / `~/.jimu/settings.json` 中的 `mcp_servers`、`extensions` 与 `--mcp-config`/`--mcp-server`）、全量 11 个内置工具（含 `--tools` / `--disable-tools` / `--no-tools`）、延迟工具网关（`tool_search` / `call_tool`）、Codemode、JS 扩展自动发现（`~/.jimu/extensions/*.js` 与 `<cwd>/.jimu/extensions/*.js`）、Skills、`AGENTS.md` 发现、默认系统提示词以及统一的 `~/.jimu/sessions/` 会话持久化：

```bash
# 默认启动交互 TUI（指定模型、思考强度与权限档位）
cargo run --release -p jimu -- \
  --provider deepseek -m deepseek-flash --api-key-env TEST_DEEPSEEK_API_KEY \
  --thinking high --permission-mode default

# 带初始消息启动 / 续接上次会话 / 选会话恢复
jimu "帮我排查构建失败的原因"
jimu -c
jimu -r

# 非交互一次性执行（纯文本 / JSONL 事件流）
jimu -p "总结当前仓库结构"
jimu -p --mode json "总结当前仓库结构"

# 无头构建（零 jimu-tui 依赖）
cargo build --release -p jimu --no-default-features
```

TUI 权限模式保留 3 档（`default` / `acceptEdits` / `bypassPermissions`，可用 `Shift+Tab` 或 `/permissions` 实时切换），默认开启 `!` 本地快捷 Shell 模式（绑定当前工作目录）与 `~/.jimu/history.jsonl` 跨会话输入历史持久化，同时支持 `/fast [on|off]`、`/effort`、`/model`、`/resume`、`/new`、`/rewind`（双击 `Esc`）、`/multiline`、`/edit`、`/history`、`/copy`、`/export`、`/theme`、`/reload` 等全部命令，并提供两类零阻塞定制能力：
- **外部 `status_line` Shell 命令 & JSON 主题**：在 `~/.jimu/settings.json` 中配置 `show_header` 与 `status_line`（`{ "command", "refresh_ms", "timeout_ms" }`），子进程以独立进程组在后台异步执行（stdin 接收标准状态 JSON，stdout 解析完整 ANSI SGR 样式，超时/退出强制 `SIGKILL` 回收进程组，渲染热路径 O(1) 读缓存）；将 `<name>.json` 放入 `~/.jimu/themes/` 或 `<cwd>/.jimu/themes/`（项目级覆盖用户级）即可直接通过 `--theme` 与 `/theme` 选用。
- **JS UI Mod 异步桥接（`UiModBridge`）**：当加载的 QuickJS 扩展注册了 `jimu.on("ui.render", (e, next) => ...)` 或 `jimu.ui.theme(...)` 时，宿主在后台任务以 Latest-Only 合并执行 `render_ui` 并将纯数据 `UiNode` 翻译为 `ratatui` `Line` / `SurfaceDecoration` / `Span` 缓存供 `ChromeSlots` 同步读取（未注册 `ui.render` 时零后台任务开销；`StatusLine` 支持对 `status_line` Shell 输出二次洋葱包裹或整块替换）。首次启动时自动将内置用户使用手册同步到 `~/.jimu/docs/`，并在默认系统提示词中按需索引，用户可直接向 Jimu 询问配置、模型、Skills 或扩展用法。
