# Pikra 仓库维护规范

Pikra 是面向全端嵌入（Rust / Swift / 跨进程 RPC / 未来其他语言 SDK）的超轻量、模块化、无头（Headless）Agent 运行时引擎。

## 1. 先读与文档同步规约

改动任何模块前先读对应文档；**凡是涉及公开 API、工具参数、配置项、分层边界或协议行为的改动，必须在同一次提交里同步更新下列文档**，绝不允许代码与文档脱节：

- `AGENTS.md`（本文件）：仓库维护红线、分层边界、缓存与并发不变量、测试与提交规约。
- `docs/architecture.md`：整体分层拓扑、设计理念、为什么这么切边界、核心状态机与体积控制机制。
- `docs/guide-rust.md`：Rust 进程内接入指南（按层选包、内置工具与覆盖、Skills、动态工具与 MCP、拦截钩子、`pikra-durable`）。
- `docs/guide-swift.md`：Swift 6 `PikraKit`（macOS / iOS）接入指南（模型与订阅登录、按需工具裁剪、审批与脱敏、后台唤醒）。
- `docs/guide-acp.md`：`pikra-acp` 标准 Agent Client Protocol（Zed / JetBrains / Neovim 等）接入指南。
- `docs/guide-rpc.md`：`pikra --mode rpc` 跨语言子进程 JSONL 通信协议指南。

---

## 2. 核心理念与职责边界

1. **只做单会话无头内核，不越界做多 Agent 宿主**：
   - **归 Pikra 管的**：多模型流式协议、回合循环、当前会话级工具（文件、补丁、`rg` 检索、前后台交互终端、`monitor` 监听、`sleep`、`web_fetch`）、Skills 发现与增量同步、延迟工具网关（`tool_search` / `call_tool` / `exec`）、上下文压缩、会话级后台事件唤醒。
   - **归上层产品宿主（如 Dora / Meerkat）管的**：跨天持久化 Cron 定时调度、多 Agent 派生与汇报（Workers / 小弟）、UI 交互弹窗（如选择题卡片 `ask_user`、权限审批弹窗）、系统通知。严禁把具体产品的 UI 或全局调度塞进内核。
2. **内置工具单源实现 + 按需裁剪，拒绝多语言重复造轮子**：
   - 所有标准内置工作区工具只在 `pikra-tools`（Rust）实现一份。
   - 各语言 SDK 只做薄绑定（每个内置工具暴露独立构造入口，保证链接器 `-dead_strip` 能自动剔除未引用工具及其重型依赖，如 `ripgrep` 正则引擎或 HTML 解析库），并通过执行前后拦截钩子（审批、环境变量注入、输出脱敏）做宿主定制；只有宿主平台专属能力（如 macOS 日历、窗口控制）才在宿主语言编写。
3. **纯库中立（零品牌目录硬编码）+「默认值优先、全项可配」**：
   - Pikra 是底层无头库，不是某个固定品牌的终端产品。**严禁**在任何库级包（`crates/*` 除独立 CLI 外、以及 `swift/Sources/PikraKit`）中硬编码 `.pikra`、`~/.pikra` 等特定产品目录名；库层默认只识别跨生态公共标准（如 `.agents/skills`、`~/.agents`、`AGENTS.override.md`、`AGENTS.md`），宿主自有目录一律通过配置参数传入。
   - **减少写死的策略逻辑**：凡涉及目录范围、向上回溯边界（如停 `.git` 根 vs 爬到 `/` vs 仅当前目录）、候选文件名、体积/字节预算上限（支持传 `None` 不限上限）、去重开关等策略项，一律采用**合理默认值优先 + 全字段开放自定义覆盖**。
4. **双轨网络栈**：
   - Linux / Windows / CLI / 服务端使用 `pikra-http`（`reqwest` + `rustls` + 系统原生根证书库），纯静态编译零 OpenSSL 动态库依赖。
   - macOS / iOS 原生 SDK（`pikra-ffi` + `PikraKit`）不引入 `pikra-http`/`rustls`，通过统一 `HttpTransport` 桥接系统原生无痕 `URLSession`，自动继承系统 VPN/代理分流与 iOS 网络权限。
5. **未正式发布阶段：零兼容、零迁移、零死代码**：
   - 接口、存储格式或参数不合理时直接改为最干净的终态，彻底删除旧实现，严禁留向后兼容垫片、旧字段 fallback 或无用死代码。

---

## 3. 包依赖单向红线与平台限制

每个 Crate 必须保持独立可用，严禁反向依赖或跨层耦合：

| Crate | 职责边界 | 硬性依赖与平台红线 |
| :--- | :--- | :--- |
| `pikra-schema` | JSON Schema 构建、三遍松散参数纠偏、内置 NFA `pattern` 校验 | **仅** `serde`、`serde_json`（严禁引 `regex`、`thiserror`） |
| `pikra-ai` | 多模型流式线协议、跨模型消息 Handoff、Codex OAuth PKCE、`HttpTransport` 抽象 | **零** `tokio`、**零** `reqwest`、**零** TLS；100% 兼容 iOS / Wasm |
| `pikra-http` | 基于 `reqwest` + `rustls`（含系统根证书）的流式传输实现 | 仅独立 Rust 宿主与 `pikra-rpc` 引用；**严禁**被 `pikra-ffi` 引用 |
| `pikra-agent` | 纯内存回合循环、并发/串行工具调度、`steer`/`follow_up`/`abort`、5 阶段 Middleware | 零 `tokio`、零磁盘与子进程调用 |
| `pikra-codemode` | 受限 QuickJS（`rquickjs` 纯解释模式，禁 JIT 满足 iOS App Store 合规）工具编排沙盒 | 零网络、零文件系统暴露，强制内存上限与执行超时 |
| `pikra-session` | 追加式会话树（内存 / JSONL）、分支 `fork`、具名 `<system-reminder>` 去重、保前缀压缩 | 仅依赖 `pikra-agent` |
| `pikra-durable` | 可选单机 SQLite WAL（`IMMEDIATE` + `synchronous=FULL`）事务级 Checkpoint、租约隔离与恢复 | 仅无外部调度器的嵌入式守护进程选用 |
| `pikra-js` | QuickJS 动态扩展运行时（注册自定义工具与回合/工具级钩子，支持热重载） | 流式高频 `delta` 事件严禁跨 JS 边界 |
| `pikra-tools` | 模块化内置工具箱（`fs`、`patch`、`search`、`shell`、`monitor`、`sleep`、`web-fetch` + Schema 覆盖） | 子进程工具（`exec_command`、`write_stdin`、`monitor`）必须用 `#[cfg(not(target_os = "ios"))]` 隔离，确保全仓库可通过 `aarch64-apple-ios` 编译 |
| `pikra-kit` | 高阶无头会话组装（可扩展 `SkillCatalog` + 增量 Diff、BM25 `tool_search`、MCP、后台唤醒收件箱） | 默认关 `stdio` 时零 `tokio` |
| `pikra-acp` | 标准 Agent Client Protocol（ACP v1 + 编辑器扩展）库与独立二进制 `pikra-acp` | `stdout` 仅输出 JSON-RPC 2.0 报文；**严禁**被 `pikra-ffi` 引用 |
| `pikra-ffi` | C-ABI 桥接层（供 Swift `PikraKit` 等原生 SDK 链接） | **严禁**引入 `pikra-http` / `reqwest` / `rustls` / `pikra-acp` |

---

## 4. Prompt Cache 前缀稳定与核心不变量（铁律）

任何涉及提示词、工具、Skills、动态上下文、会话恢复或压缩的改动，必须逐条核对：

1. **前缀字节恒定**：
   - 会话一旦建立，基础指令与顶层常驻 `tools` 列表（含参数属性顺序，`serde_json/preserve_order`）整场会话保持字节级不变。
   - 动态工具增减一律走延迟网关（`deferred_tools` 上下文块 + `tool_search` + `call_tool`），绝不改写顶层 `tools` 数组。
2. **动态上下文与 Skills 增量更新**：
   - `set_context(name, text)` 同名同值严格去重，变化时只在两轮之间以 `<system-reminder>` 追加在对话末尾，绝不回改头部系统指令。
   - **Skills 增量 Diff**：仅在新开会话和压缩后发送一次完整 `<available_skills>`（全量渲染所有可见 Skills，不做预算截断或路径别名压缩）；会话中途增删 Skill 只在末尾追加 `# Skills changed` 增删增量，单纯修改已有 `SKILL.md` 正文静默不刷屏（模型通过 `read` 直接读最新磁盘文件）。
3. **压缩（Compaction）与 Token 估算**：
   - 自动压缩生成摘要时，原样复用父请求完整前缀并在末尾追加摘要指令，最大化命中服务端缓存读取。
   - Token 估算必须排除图片原始 base64 字节（单图按固定常量如 `1_200` tokens 计），输出预留上限封顶，防止 128k 输出上限模型过早触发压缩。
4. **线协议与模型兼容硬约束**：
   - **Anthropic Messages**：并行工具结果必须合并进同一条 `role: "user"` 消息；`is_error: true` 结果里的图片拆成后续独立图片块；Claude 4.7+/5.x 自动走 `adaptive` 思考模式，旧模型手动 `budget_tokens` 在 `max_tokens <= 1024` 时自动跳过思考块防 400 报错；保留根级 `$ref`/组合器工具 Schema。
   - **OpenAI Responses / Completions**：同模型保留 `item_id` 与加密推理签名，跨模型切换时自动剥离私有签名降级为纯文本；Completions 工具结果中的图片拆为紧随的用户多模态消息块保留视觉上下文。
   - **助手正文纯净**：严禁往助手 `text` 块拼接长度截断等系统提示，截断一律通过 `StopReason::Length` 表达。
5. **进程、安全与取消收尾**：
   - **零孤儿进程**：所有子进程（`exec_command`、`monitor`、stdio MCP）必须独立成进程组，超时、取消或会话关闭时对整个负进程组发送 `SIGKILL` 并回收句柄。
   - **取消收尾义务**：`abort()` 即使在事件流未被外部轮询时也必须推进底层取消收尾，清理未消费的 `steer` 队列，确保紧接着的下一轮调用绝不误报 `Busy`。
   - **网络与文件安全**：Swift `URLSession` 必须使用无痕会话（`ephemeral`、`urlCache = nil`）；`web_fetch` 默认拦截环回、局域网与云元数据内网地址（含重定向跳数检查）；`write`/`edit`/`apply_patch` 遇到软链接时写入真实目标文件而非破坏软链接本身。

---

## 5. 测试、代码风格与提交规范

- **永久禁止单元测试**：**严禁**在任何 `src/` 文件中出现 `#[cfg(test)]`（合入前必跑 `! rg -n "#\[cfg\(test\)\]" crates/*/src apps/*/src`）。
- **只写极简核心业务 E2E 测试**：统一放在 `crates/<crate>/tests/*_e2e.rs` 或 `swift/Tests/`，每个核心能力 1~3 条端到端主干用例，新增测试代码不超过产品代码的一半。
- **测试零时间依赖**：**严禁**在测试里用 `sleep` 轮询或硬编码耗时阈值，一律使用通道、事件通知或确定性 Mock Transport。
- **零告警门禁**：
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - 若改动 `pikra-ffi` 或 `swift/`：必须跑 `./swift/scripts/build-xcframework.sh --all-platforms --zip` 与 `swift test --package-path swift`，确认 macOS 与 iOS 真机/模拟器全架构零警告通过。
- **Git 卫生**：
  - 严禁提交密钥、`.env`、`target/`、`.build/` 或构建产物 `*.xcframework`。
  - 严禁 `git add .` / `git add -A`，必须按明确路径暂存提交：`git commit -m "..." -- <paths>`；Commit 信息禁止写 `Closes #N` / `Fixes #N`。
  - 默认仅本地提交，未经明确指示严禁 `git push`。
