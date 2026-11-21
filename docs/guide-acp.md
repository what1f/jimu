# Pikra ACP（Agent Client Protocol）接入指南

`pikra-acp` 是 Pikra 官方提供的 **Agent Client Protocol（ACP v1 + 编辑器增强扩展）** 适配器与独立命令行程序，可直接接入 **Zed**、**JetBrains 全家桶**、**Neovim（CodeCompanion / Avante）**、**Emacs（agent-shell）**、**Toad** 等所有兼容 ACP 的编辑器与客户端。

---

## 1. 编译与安装

在仓库根目录编译独立二进制：

```bash
cargo build --release -p pikra-acp
# 产物位于 ./target/release/pikra-acp
# （注：主程序 `pikra --mode acp` 同样支持直接进入 ACP 模式）
```

### 认证方式（任选其一）

1. **环境变量 API Key（零配置自动识别）**：
   按优先级自动识别 `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`GEMINI_API_KEY`（或 `GOOGLE_API_KEY`）、`DEEPSEEK_API_KEY`、`OPENROUTER_API_KEY`。
2. **ChatGPT / Codex 订阅账号登录（OAuth PKCE）**：
   ```bash
   pikra-acp login
   ```
   登录凭证以 `0600` 权限保存在 `~/.pikra/auth.json`（若本机已通过 Codex CLI 登录过 `~/.codex/auth.json`，也会自动只读复用并在刷新时写入 `~/.pikra/auth.json`）。也可以直接在编辑器面板点击登录按钮触发内置终端登录流程。

---

## 2. 主流编辑器与客户端配置

### 2.1 Zed（`~/.config/zed/settings.json`）

```json
{
  "agent_servers": {
    "Pikra": {
      "command": "/path/to/pikra/target/release/pikra-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "sk-ant-..."
      }
    }
  }
}
```

> 若已运行 `pikra-acp login` 登录 ChatGPT/Codex 订阅或在系统环境配好了密钥，`env` 可留空 `{}`；进入会话后可直接在 Zed 顶部下拉框切换 **模型（Model）**、**思考强度（Thought Level）**、**极速模式（Fast）** 与 **权限模式（Mode）**。

### 2.2 JetBrains IDEs（`~/.jetbrains/acp.json`）

```json
{
  "agent_servers": {
    "Pikra": {
      "command": "/path/to/pikra/target/release/pikra-acp",
      "args": ["--thinking", "high"],
      "env": {}
    }
  }
}
```

### 2.3 Neovim（`codecompanion.nvim`）

```lua
require("codecompanion").setup({
  adapters = {
    acp = {
      pikra = function()
        return require("codecompanion.adapters").extend("claude_code", {
          name = "pikra",
          formatted_name = "Pikra",
          commands = {
            default = {
              "/path/to/pikra/target/release/pikra-acp",
            },
          },
        })
      end,
    },
  },
})
```

---

## 3. 已支持的完整 ACP 特性矩阵

| 能力域 | 具体支持表现 |
| :--- | :--- |
| **编辑器未保存缓冲区与内联 Diff** | 当客户端声明 `fs.readTextFile` / `fs.writeTextFile` 时，`read` 直接读取编辑器当前未保存内容，`write` / `edit` / `apply_patch` 在审批前与完成后均输出标准 `Diff` 与行号定位 `ToolCallLocation`，在 IDE 内直接渲染双栏或行内对比 |
| **IDE 内嵌实时终端** | 当客户端声明 `terminal` 能力时，终端命令自动通过 `terminal/create` 在编辑器面板内嵌实时输出流，并在取消或超时时自动 `terminal/kill` + `terminal/release`；无此能力的客户端自动无缝回退至 Pikra 本地终端引擎 |
| **四档权限模式与交互审批** | 内置 `default`（读写与命令走 `session/request_permission` 弹窗，支持单次允许 / 本会话始终允许 / 拒绝）、`acceptEdits`（工作区内改文件自动放行，跑命令弹窗确认）、`plan`（只读探索出方案，拦截写操作）与 `bypassPermissions`（全自动放行） |
| **会话全生命周期持久化** | 完整实现 `session/new`、`session/load`（严格按序回放历史消息、思考、工具卡片与压缩记录）、`session/resume`（免回放秒连）、`session/list`（按工作目录筛选与游标分页）、`session/fork`、`session/delete`、`session/archive`、`session/set_title`（首轮自动生成会话标题） |
| **流中插话（`_session/steering`）与随时取消** | 兼容 `codex-acp` / `claude-agent-acp` 的 `_session/steering` 扩展（运行中实时注入指令，空闲时支持 `promptRequired`，携带 `"isMeta": true` 时作为仅模型可见的隐藏提醒注入且不在 `session/load` 回放用户气泡）；收到 `session/cancel` 立即中断流式生成、在途终端与审批等待并返回 `cancelled` |
| **动态面板配置、MCP 与斜杠命令** | 原生暴露模型、思考档位、`fast` 模式与权限模式选择器；自动挂载客户端传入的 `mcpServers`（`stdio` + `http`）；自动将 `/compact`、`/plan`、`/reload-skills` 及项目 `.agents/skills` / `.pikra/skills` 注册为编辑器 `/` 快捷命令 |
| **可配置 `AGENTS.md` 与目录边界** | 默认加载 `~/.agents` 与工作区层级的 `AGENTS.override.md` / `AGENTS.md`（自动兼容客户端传入的 `additionalDirectories` 并对完全一致的正文自动去重）；支持通过 `--walk-boundary <project-root\|filesystem-root\|cwd-only>`、`--instruction-max-bytes <bytes\|none>`、`--instruction-file <path>`、`--no-user-instructions`、`--no-instructions`、`--pikra-home <dir>` 按需定制 |

---

## 4. 在 Rust 宿主中直接作为库嵌入

除了独立子进程运行，`pikra-acp` 也可在任意异步管道（如内存双工流、WebSocket 桥接或测试桩）上直接运行：

```rust
use pikra_acp::{AcpConfig, AcpServer, serve_io};
use std::sync::Arc;

pub async fn serve_custom_stream<R, W>(reader: R, writer: W) -> Result<(), String>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let server = Arc::new(AcpServer::new(AcpConfig::default())?);
    serve_io(server, reader, writer).await
}
```
