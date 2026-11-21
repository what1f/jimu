# Pikra Swift SDK (`PikraKit`) 接入指南

`PikraKit` 是面向 macOS 15+ 与 iOS 17+ 的零第三方依赖 Swift 6 原生 SDK（底层链接 `PikraCore.xcframework`）。网络直接复用系统无痕 `URLSession`，内置工具与 Skills/动态工具能力由跨平台 Rust 内核统一驱动。

---

## 1. 引入依赖

### 本地路径引入（Monorepo / 开发联调）
先在 Pikra 仓库构建多平台静态库框架：
```bash
./swift/scripts/build-xcframework.sh --all-platforms --zip
```
在你的 `Package.swift` 中引入：
```swift
dependencies: [
    .package(path: "../pikra/swift"),
],
targets: [
    .target(
        name: "MyAppAgent",
        dependencies: [.product(name: "PikraKit", package: "PikraKit")]
    ),
]
```

---

## 2. 创建会话与流式对话

```swift
import Foundation
import PikraKit

let model = PikraModel(
    id: "claude-sonnet-5-5",
    provider: "anthropic",
    api: .anthropicMessages,
    baseURL: "https://api.anthropic.com/v1",
    contextWindow: 200_000,
    maxOutputTokens: 16_384,
    supportsImages: true,
    supportsReasoning: true
)

let env = try PikraWorkspaceEnvironment(workspace: workspaceURL)
let tools = [
    try PikraBuiltinTools.read(env),
    try PikraBuiltinTools.edit(env),
    try PikraBuiltinTools.grep(env),
    try PikraBuiltinTools.webFetch(env),
]

var config = PikraConfiguration(
    model: model,
    systemPrompt: "You are a helpful macOS assistant.",
    tools: tools,
    thinkingLevel: .medium,
    persistenceURL: workspaceURL.appendingPathComponent(".my-app/session.jsonl"),
    cwd: workspaceURL.path
)
config.workspaceEnvironment = env
config.authorizeRequest = { req in
    req.setValue("Bearer \(apiKeyFromKeychain())", forHTTPHeaderField: "Authorization")
}

let agent = try await PikraAgent.open(configuration: config)
let stream = try await agent.prompt("帮我看看当前目录有哪些 Swift 文件")

for try await event in stream {
    switch event {
    case .textDelta(_, let delta):
        print(delta, terminator: "")
    case .toolStarted(_, let name, let args):
        print("\n[Tool] \(name) \(args)")
    case .turnEnded:
        print("\n--- Done ---")
    default:
        break
    }
}
```

### ChatGPT / Codex 订阅账号登录（OAuth PKCE）
无需手工管理 Token 刷新，直接使用内置 `PikraCodexOAuth` 与 `PikraCodexSubscription`（支持多账号并发隔离，自动防旧账号刷新覆盖新账号凭证）：
- 生成 PKCE 授权链接并在回调后换取凭证；
- 将 `PikraCodexSubscription` 挂入 `PikraConfiguration`，使用 `.openaiCodexResponses` 协议即可直连订阅模型。

---

## 3. 选用内置工具与编写 Swift 自定义工具

### 3.1 按需挂载内置工作区工具（自动死代码裁剪）
内置工具底层全部由 Rust `pikra-tools` 单一实现驱动，每个工具对应独立的构造入口：
- **打包自动瘦身**：Xcode Release 编译默认开启 `-dead_strip`，你的代码里没引用的重型工具（如内嵌 `ripgrep` 的 `grep` 或内嵌 Defuddle HTML 解析器的 `webFetch`）会在最终打 `.app` 包时被链接器整块剔除。
- **平台边界**：`read`、`write`、`edit`、`applyPatch`、`find`、`grep`、`sleep`、`webFetch` 在 **macOS 与 iOS 全可用**；涉及子进程的 `bash` / `execCommand` / `writeStdin` / `monitor` 仅在 **macOS** 可用（iOS 编译期自动隔离）。
- **定制描述与 Schema**：每个内置工具均支持传入覆盖配置（改工具名、替换/追加描述文案、隐藏特定可选参数并固定默认值）。

### 3.2 用 Swift 闭包注册宿主专属工具
涉及 macOS / iOS 系统原生 API（如读取日历、控制窗口、剪贴板）的工具，直接用 `PikraTool` 编写，可自由选择设为常驻（`deferred: false`）或延迟加载（`deferred: true`）：

```swift
let clipboardTool = PikraTool(
    name: "read_clipboard",
    description: "Read plain text from the macOS system pasteboard.",
    parameters: [
        "type": "object",
        "properties": [:],
    ],
    deferred: true
) { _, _ in
    .text(NSPasteboard.general.string(forType: .string) ?? "")
}
```

### 3.3 权限审批弹窗与输出脱敏
通过 `PikraToolApproval`（或执行前后拦截回调）集中处理高危操作确认与密钥脱敏，无需自己重包工具逻辑：
- 只读工具（`read` / `find` / `grep`）可直接放行；
- 写入与终端工具（`write` / `edit` / `applyPatch` / `execCommand`）在执行前挂起等待用户在 UI 审批卡片点击允许或拒绝；用户取消回复时审批等待会自动响应 Swift Concurrency `CancellationError` 立即退出。

---

## 4. Skills、动态工具与后台唤醒最佳实践

1. **`AGENTS.md` 规则与 Skills 目录配置**：
   - 库层零品牌目录硬编码：默认只识别跨生态 `.agents/skills` 与 `~/.agents`，宿主通过 `userDirs` / `projectSkillDirs` 挂载自有目录（如 `~/.dora`、`.dora/skills`）；自动化测试中可将 `includeUserInstructions` / `includeUserSkills` 设为 `false` 隔离本机全局目录。
   - 共用 `PikraWalkBoundary`（`.projectRoot` 停在 `.git` 根、`.filesystemRoot` 一路爬到 `/`、`.currentDirOnly` 仅当前目录）；`PikraInstructionDiscovery.Configuration` 默认开启相同正文自动去重与 `32 KiB` 可调上限（传 `nil` 不限上限）。
   - `AGENTS.md` 或 Skills 文件变更时分别调用 `reloadInstructions()` / `reloadSkills()`；Skills 只追加 `# Skills changed` 增删增量，不会重发整份目录破坏前缀缓存。
2. **MCP 与海量工具热更新**：
   - 会话运行期增删外部 MCP 工具调用 `upsertTools` / `removeTools`（仅操作延迟工具池，保证顶层常驻工具字节不变）。
3. **隐藏系统上下文（`isMeta`）、中途插话与生命周期**：
   - **仅模型可见的隐藏消息**：轮前排队用 `try await agent.setContext(name:text:)`（去重）、`try await agent.appendContext(_:name:)` 或无标签包装的 `try await agent.appendMeta(_:)`；无用户发言直接触发隐藏轮次用 `try await agent.prompt("...", isMeta: true)`；运行中注入隐藏纠偏用 `await agent.steer(text: "...", isMeta: true)`（不会触发 `.steered` 用户气泡事件）或 `try await agent.followUp(text: "...", isMeta: true)`；读取 `agent.entries()` 时可直接用 `entry.isMeta` 过滤 UI 展示。
   - **用户插话与停止**：回复进行中用户发新消息可调 `agent.steer(text:)` 实时插话，或调 `agent.followUp(text:)` 排入下一轮；用户点击停止调 `await agent.interrupt()`，该函数会等待在途工具取消与未读插话清理完毕后才返回，随后可立即安全发起下一轮 `prompt`。
   - 会话不用时调 `await agent.close()`（或释放引用），底层自动回收该会话启动的所有后台子进程组与 `Monitor`。
