# Pikra

用 Rust 构建的超轻量、模块化、Prompt Cache 友好的无头（Headless）Agent 运行时引擎，内置单源可裁剪的产品级工具箱、可扩展 Skills 与 BM25 动态工具网关，原生提供 **Rust Crates**、零外部依赖的 **Swift 6 SDK (`PikraKit`)** 与跨语言 **JSONL RPC (`pikra --mode rpc`)**。

---

## 核心特性

- **多协议无损流式引擎 (`pikra-ai`)**：统一支持 OpenAI Responses、OpenAI Codex（ChatGPT 订阅 OAuth PKCE）、Chat Completions、Anthropic Messages（含 Claude 4.7+/5.x Adaptive Thinking）与 Google Gemini，会话中途切换模型自动完成私有思考签名降级。
- **Prompt Cache 一等公民 (`pikra-session` + `pikra-kit`)**：锁定基础指令与常驻工具字节顺序；动态上下文块去重尾部追加；压缩请求复用父前缀命中缓存读取；Skills 热更新采用增量 Diff。
- **单源实现、按需裁剪的产品级工具箱 (`pikra-tools`)**：
  - 文件与补丁：`read`（文本+多模态图片）、`write`、4 级容错行匹配的 `edit`（支持多处替换）与 Codex 同款 `apply_patch`；
  - 高速本地检索：内嵌 `ripgrep` 核心库的 `find` 与 `grep`（自动遵循 `.gitignore`，无需终端审批，iOS 沙盒可用）；
  - 前后台统一终端与监听：支持 PTY 交互与自动转后台的 `exec_command` + `write_stdin`、按行防抖与刷屏熔断唤醒的 `monitor`、可提前打断的 `sleep`；
  - 网页正文提纯：移植自 Defuddle + Turndown + 站点路由（含 X 推文/长文）的 `web_fetch`（内置 SSRF 内网拦截，桌面走系统根证书 `reqwest`，苹果端自动走无痕 `URLSession`）；
  - 支持按工具覆盖名称、描述与裁剪参数 Schema，未引用工具在 App 链接打包时自动死代码剔除。
- **可扩展 Skills 与动态工具编排 (`pikra-kit` + `pikra-codemode`)**：默认兼容 `.agents/skills`（支持向上回溯仓库根与 Kitter 软链接）及自定义品牌目录，内置 8KB 预算保护、BM25 `tool_search`、超阈值自动转延迟与受限 QuickJS 批量工具编排沙盒。

---

## 文档导航

- **[仓库维护规范 (`AGENTS.md`)](./AGENTS.md)**：分层红线、缓存与并发不变量、测试铁律与提交流程（参与开发必读）。
- **[架构与设计边界 (`docs/architecture.md`)](./docs/architecture.md)**：包依赖拓扑、引擎与宿主分工边界、核心状态机设计。
- **[Rust 接入指南 (`docs/guide-rust.md`)](./docs/guide-rust.md)**：按层选包、工具定制、Skills 配置、后台唤醒与拦截切面。
- **[Swift SDK (`PikraKit`) 接入指南 (`docs/guide-swift.md`)](./docs/guide-swift.md)**：macOS / iOS 集成、订阅登录、工具裁剪与权限审批。
- **[JSONL RPC 跨语言接入指南 (`docs/guide-rpc.md`)](./docs/guide-rpc.md)**：`pikra --mode rpc` 命令与事件流协议。

---

## 快速构建与验证

```bash
# 1. Rust 全量端到端测试与零警告检查
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features

# 2. 打包全平台 PikraCore.xcframework (macOS + iOS + Simulator) 并跑 Swift 6 E2E 测试
./swift/scripts/build-xcframework.sh --all-platforms --zip
swift test --package-path swift

# 3. 构建独立 CLI / RPC 二进制
cargo build --release -p pikra-rpc
```
