import Foundation

/// Errors thrown by `JimuAgent` and `JimuKit` operations.
public enum JimuError: Error, Sendable, Equatable, LocalizedError {
    case invalidArgument(String)
    case busy
    case closed
    case notFound(String)
    case io(String)
    case cancelled
    case transport(String)
    case internalError(String)

    public var errorDescription: String? {
        switch self {
        case .invalidArgument(let msg): return "Invalid argument: \(msg)"
        case .busy: return "Session is currently busy"
        case .closed: return "Session is closed"
        case .notFound(let msg): return "Not found: \(msg)"
        case .io(let msg): return "I/O error: \(msg)"
        case .cancelled: return "Operation cancelled"
        case .transport(let msg): return "Transport error: \(msg)"
        case .internalError(let msg): return "Internal error: \(msg)"
        }
    }
}

/// Supported wire protocol adapters in `jimu-ai`.
public enum JimuApiKind: Sendable, Equatable, Hashable, Codable {
    case openaiResponses
    case openaiCodexResponses
    case openaiCompletions
    case anthropicMessages
    case googleGenerativeAi
    case custom(String)

    public var rawValue: String {
        switch self {
        case .openaiResponses: return "openai-responses"
        case .openaiCodexResponses: return "openai-codex-responses"
        case .openaiCompletions: return "openai-completions"
        case .anthropicMessages: return "anthropic-messages"
        case .googleGenerativeAi: return "google-generative-ai"
        case .custom(let name): return name
        }
    }

    public init(rawValue: String) {
        switch rawValue {
        case "openai-responses": self = .openaiResponses
        case "openai-codex-responses": self = .openaiCodexResponses
        case "openai-completions": self = .openaiCompletions
        case "anthropic-messages": self = .anthropicMessages
        case "google-generative-ai": self = .googleGenerativeAi
        default: self = .custom(rawValue)
        }
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.singleValueContainer()
        let raw = try container.decode(String.self)
        self.init(rawValue: raw)
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// Reasoning / thinking effort level.
public enum JimuThinkingLevel: String, Codable, Sendable, Equatable, Hashable {
    case off
    case none
    case minimal
    case low
    case medium
    case high
    case xhigh
    case max
}

/// Prompt cache retention hint.
public enum JimuCacheRetention: String, Codable, Sendable, Equatable, Hashable {
    case none
    case short
    case long
}

/// Tool batch execution strategy.
public enum JimuToolExecutionMode: String, Codable, Sendable, Equatable, Hashable {
    case parallel
    case sequential
}

/// Queue drain policy for steering and follow-up messages.
public enum JimuQueueMode: String, Codable, Sendable, Equatable, Hashable {
    case oneAtATime = "one-at-a-time"
    case all = "all"
}

/// Model pricing (USD per 1M tokens).
public struct JimuModelCost: Codable, Sendable, Equatable {
    public var input: Double
    public var output: Double
    public var cacheRead: Double
    public var cacheWrite: Double

    public init(
        input: Double = 0,
        output: Double = 0,
        cacheRead: Double = 0,
        cacheWrite: Double = 0
    ) {
        self.input = input
        self.output = output
        self.cacheRead = cacheRead
        self.cacheWrite = cacheWrite
    }

    enum CodingKeys: String, CodingKey {
        case input
        case output
        case cacheRead = "cache_read"
        case cacheWrite = "cache_write"
    }
}

/// LLM model descriptor passed to `JimuConfiguration` or `setModel(_:)`.
public struct JimuModel: Sendable, Equatable, Codable {
    public var id: String
    public var name: String
    public var provider: String
    public var api: JimuApiKind
    public var baseURL: String
    public var contextWindow: UInt32
    public var maxOutputTokens: UInt32
    public var supportsImages: Bool
    public var supportsReasoning: Bool
    public var cost: JimuModelCost
    public var headers: [(String, String)]

    public init(
        id: String,
        name: String? = nil,
        provider: String,
        api: JimuApiKind,
        baseURL: String,
        contextWindow: UInt32 = 128_000,
        maxOutputTokens: UInt32 = 8_192,
        supportsImages: Bool = false,
        supportsReasoning: Bool = false,
        cost: JimuModelCost = JimuModelCost(),
        headers: [(String, String)] = []
    ) {
        self.id = id
        self.name = name ?? id
        self.provider = provider
        self.api = api
        self.baseURL = baseURL
        self.contextWindow = contextWindow
        self.maxOutputTokens = maxOutputTokens
        self.supportsImages = supportsImages
        self.supportsReasoning = supportsReasoning
        self.cost = cost
        self.headers = headers
    }

    public static func == (lhs: JimuModel, rhs: JimuModel) -> Bool {
        lhs.id == rhs.id &&
            lhs.name == rhs.name &&
            lhs.provider == rhs.provider &&
            lhs.api == rhs.api &&
            lhs.baseURL == rhs.baseURL &&
            lhs.contextWindow == rhs.contextWindow &&
            lhs.maxOutputTokens == rhs.maxOutputTokens &&
            lhs.supportsImages == rhs.supportsImages &&
            lhs.supportsReasoning == rhs.supportsReasoning &&
            lhs.cost == rhs.cost &&
            lhs.headers.count == rhs.headers.count &&
            zip(lhs.headers, rhs.headers).allSatisfy { $0.0 == $1.0 && $0.1 == $1.1 }
    }

    enum CodingKeys: String, CodingKey {
        case id
        case name
        case provider
        case api
        case baseURL = "base_url"
        case contextWindow = "context_window"
        case maxOutputTokens = "max_output_tokens"
        case supportsImages = "supports_images"
        case supportsReasoning = "supports_reasoning"
        case cost
        case headers
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        provider = try c.decode(String.self, forKey: .provider)
        api = try c.decode(JimuApiKind.self, forKey: .api)
        baseURL = try c.decode(String.self, forKey: .baseURL)
        contextWindow = try c.decode(UInt32.self, forKey: .contextWindow)
        maxOutputTokens = try c.decode(UInt32.self, forKey: .maxOutputTokens)
        supportsImages = try c.decode(Bool.self, forKey: .supportsImages)
        supportsReasoning = try c.decode(Bool.self, forKey: .supportsReasoning)
        cost = try c.decodeIfPresent(JimuModelCost.self, forKey: .cost) ?? JimuModelCost()
        let rawHeaders = try c.decodeIfPresent([[String]].self, forKey: .headers) ?? []
        headers = rawHeaders.compactMap { pair in
            guard pair.count == 2 else { return nil }
            return (pair[0], pair[1])
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(name, forKey: .name)
        try c.encode(provider, forKey: .provider)
        try c.encode(api, forKey: .api)
        try c.encode(baseURL, forKey: .baseURL)
        try c.encode(contextWindow, forKey: .contextWindow)
        try c.encode(maxOutputTokens, forKey: .maxOutputTokens)
        try c.encode(supportsImages, forKey: .supportsImages)
        try c.encode(supportsReasoning, forKey: .supportsReasoning)
        try c.encode(cost, forKey: .cost)
        try c.encode(headers.map { [$0.0, $0.1] }, forKey: .headers)
    }
}

/// Auto/manual compaction policy configuration.
public struct JimuCompactionConfig: Codable, Sendable, Equatable {
    public var enabled: Bool
    public var reserveTokens: UInt32?
    public var keepRecentTokens: UInt64
    public var maxSummaryChars: Int
    public var customInstructions: String?

    public init(
        enabled: Bool = true,
        reserveTokens: UInt32? = nil,
        keepRecentTokens: UInt64 = 20_000,
        maxSummaryChars: Int = 24_000,
        customInstructions: String? = nil
    ) {
        self.enabled = enabled
        self.reserveTokens = reserveTokens
        self.keepRecentTokens = keepRecentTokens
        self.maxSummaryChars = maxSummaryChars
        self.customInstructions = customInstructions
    }

    enum CodingKeys: String, CodingKey {
        case enabled
        case reserveTokens = "compaction_reserve_tokens"
        case keepRecentTokens = "keep_recent_tokens"
        case maxSummaryChars = "max_summary_chars"
        case customInstructions = "custom_instructions"
    }
}

/// Resource limits for the optional QuickJS `exec` codemode sandbox.
public struct JimuCodemodeOptions: Codable, Sendable, Equatable {
    public var timeoutMs: UInt64?
    public var memoryLimitBytes: Int
    public var maxStackSize: Int

    public init(
        timeoutMs: UInt64? = 300_000,
        memoryLimitBytes: Int = 32 * 1024 * 1024,
        maxStackSize: Int = 512 * 1024
    ) {
        self.timeoutMs = timeoutMs
        self.memoryLimitBytes = memoryLimitBytes
        self.maxStackSize = maxStackSize
    }

    enum CodingKeys: String, CodingKey {
        case timeoutMs = "timeout_ms"
        case memoryLimitBytes = "memory_limit_bytes"
        case maxStackSize = "max_stack_size"
    }
}

/// Source for an optional QuickJS extension (`jimu-js`).
public enum JimuJsExtensionSource: Sendable, Equatable {
    case inline(name: String, code: String)
    case file(URL)
}

/// Configuration for optional QuickJS extensions (`jimu-js`).
public struct JimuJsExtensionConfig: Sendable, Equatable {
    public var sources: [JimuJsExtensionSource]
    public var memoryLimitBytes: Int
    public var maxStackSizeBytes: Int
    public var timeoutMs: UInt64

    public init(
        sources: [JimuJsExtensionSource],
        memoryLimitBytes: Int = 32 * 1024 * 1024,
        maxStackSizeBytes: Int = 1024 * 1024,
        timeoutMs: UInt64 = 10_000
    ) {
        self.sources = sources
        self.memoryLimitBytes = memoryLimitBytes
        self.maxStackSizeBytes = maxStackSizeBytes
        self.timeoutMs = timeoutMs
    }
}

/// Multimodal content part passed in user prompts.
public enum JimuPromptContent: Sendable, Equatable {
    case text(String)
    case image(data: Data, mimeType: String)
    case imageBase64(String, mimeType: String)

    func toWirePart() -> JimuContentPart {
        switch self {
        case .text(let s):
            return .text(s)
        case .image(let data, let mimeType):
            return .image(data: data.base64EncodedString(), mimeType: mimeType)
        case .imageBase64(let b64, let mimeType):
            return .image(data: b64, mimeType: mimeType)
        }
    }
}

/// Wire content part used in prompts and tool outputs.
public enum JimuContentPart: Codable, Sendable, Equatable {
    case text(String)
    case image(data: String, mimeType: String)

    enum CodingKeys: String, CodingKey {
        case type
        case text
        case data
        case mimeType = "mime_type"
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "text":
            self = .text(try c.decode(String.self, forKey: .text))
        case "image":
            self = .image(
                data: try c.decode(String.self, forKey: .data),
                mimeType: try c.decode(String.self, forKey: .mimeType)
            )
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: c,
                debugDescription: "Unknown content part type: \(type)"
            )
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .text(let text):
            try c.encode("text", forKey: .type)
            try c.encode(text, forKey: .text)
        case .image(let data, let mimeType):
            try c.encode("image", forKey: .type)
            try c.encode(data, forKey: .data)
            try c.encode(mimeType, forKey: .mimeType)
        }
    }
}

/// Output returned by a native Swift tool execution.
public struct JimuToolOutput: Codable, Sendable, Equatable {
    public var content: [JimuContentPart] {
        didSet {
            // Keep structuredContent synchronized when a host wrapper redacts JSON-backed
            // content; otherwise drop it so unredacted structured payloads never leak.
            guard let payload = structuredContent else { return }
            if oldValue.count == 1,
               case .text(let text) = oldValue[0],
               (try? JSONDecoder().decode(JSONValue.self, from: Data(text.utf8))) == payload,
               let updated = try? JSONDecoder().decode(JSONValue.self, from: Data(textOutput.utf8)),
               case .object = updated
            {
                structuredContent = updated
            } else {
                structuredContent = nil
            }
        }
    }
    public var details: JSONValue
    public var structuredContent: JSONValue?
    public var isError: Bool
    public var terminate: Bool

    public init(
        content: [JimuContentPart],
        details: JSONValue = .null,
        structuredContent: JSONValue? = nil,
        isError: Bool = false,
        terminate: Bool = false
    ) {
        self.content = content
        self.details = details
        self.structuredContent = structuredContent
        self.isError = isError
        self.terminate = terminate
    }

    public static func text(
        _ value: String,
        details: JSONValue = .null,
        structuredContent: JSONValue? = nil
    ) -> JimuToolOutput {
        JimuToolOutput(
            content: [.text(value)],
            details: details,
            structuredContent: structuredContent,
            isError: false,
            terminate: false
        )
    }

    public static func error(_ message: String, details: JSONValue = .null) -> JimuToolOutput {
        JimuToolOutput(
            content: [.text(message)],
            details: details,
            structuredContent: nil,
            isError: true,
            terminate: false
        )
    }

    public static func json(_ value: JSONValue) -> JimuToolOutput {
        let encoded = (try? JSONEncoder().encode(value)).flatMap { String(data: $0, encoding: .utf8) } ?? "null"
        return JimuToolOutput(
            content: [.text(encoded)],
            details: .null,
            structuredContent: value,
            isError: false,
            terminate: false
        )
    }

    public var textOutput: String {
        content.compactMap { part in
            if case .text(let t) = part { return t }
            return nil
        }.joined(separator: "\n")
    }

    enum CodingKeys: String, CodingKey {
        case content
        case details
        case structuredContent = "structured_content"
        case isError = "is_error"
        case terminate
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        content = try c.decode([JimuContentPart].self, forKey: .content)
        details = try c.decodeIfPresent(JSONValue.self, forKey: .details) ?? .null
        structuredContent = try c.decodeIfPresent(JSONValue.self, forKey: .structuredContent)
        isError = try c.decodeIfPresent(Bool.self, forKey: .isError) ?? false
        terminate = try c.decodeIfPresent(Bool.self, forKey: .terminate) ?? false
    }
}

/// Alias matching SDK documentation naming.
public typealias JimuToolResult = JimuToolOutput

/// Context passed into each host tool invocation.
public struct JimuToolCallContext: Sendable {
    public let toolCallID: String
    public let toolName: String
    private let progressHandler: @Sendable (JimuToolOutput) -> Void

    public init(
        toolCallID: String,
        toolName: String,
        onProgress: @escaping @Sendable (JimuToolOutput) -> Void = { _ in }
    ) {
        self.toolCallID = toolCallID
        self.toolName = toolName
        self.progressHandler = onProgress
    }

    /// Emit an intermediate tool progress update (`JimuEvent.toolUpdated`).
    public func reportProgress(_ partial: JimuToolOutput) {
        progressHandler(partial)
    }
}

/// Native Swift async tool registered on a `JimuAgent`.
public struct JimuTool: Sendable {
    public let name: String
    public let description: String
    public let parameters: JSONValue
    public let deferred: Bool
    public let executionMode: JimuToolExecutionMode
    public let execute: @Sendable (JimuToolCallContext, JSONValue) async throws -> JimuToolOutput
    internal var associatedEnvironment: JimuWorkspaceEnvironment?

    public init(
        name: String,
        description: String,
        parameters: JSONValue = .object([
            "type": .string("object"),
            "properties": .object([:]),
        ]),
        deferred: Bool = false,
        executionMode: JimuToolExecutionMode = .parallel,
        execute: @escaping @Sendable (JimuToolCallContext, JSONValue) async throws -> JimuToolOutput
    ) {
        self.name = name
        self.description = description
        self.parameters = parameters
        self.deferred = deferred
        self.executionMode = executionMode
        self.execute = execute
        self.associatedEnvironment = nil
    }
}

/// Per-LLM-request token usage and cost accounting.
public struct JimuUsage: Codable, Sendable, Equatable {
    public var input: UInt64
    public var output: UInt64
    public var cacheRead: UInt64
    public var cacheWrite: UInt64
    public var reasoning: UInt64?
    public var totalTokens: UInt64
    public var costTotal: Double
    public var costFromProvider: Bool

    public init(
        input: UInt64 = 0,
        output: UInt64 = 0,
        cacheRead: UInt64 = 0,
        cacheWrite: UInt64 = 0,
        reasoning: UInt64? = nil,
        totalTokens: UInt64 = 0,
        costTotal: Double = 0,
        costFromProvider: Bool = false
    ) {
        self.input = input
        self.output = output
        self.cacheRead = cacheRead
        self.cacheWrite = cacheWrite
        self.reasoning = reasoning
        self.totalTokens = totalTokens
        self.costTotal = costTotal
        self.costFromProvider = costFromProvider
    }

    enum CodingKeys: String, CodingKey {
        case input
        case output
        case cacheRead = "cache_read"
        case cacheWrite = "cache_write"
        case reasoning
        case totalTokens = "total_tokens"
        case costTotal = "cost_total"
        case costFromProvider = "cost_from_provider"
    }
}

/// Assistant turn stop reason.
public enum JimuStopReason: String, Codable, Sendable, Equatable {
    case pending
    case stop
    case length
    case toolUse
    case error
    case aborted
}

/// Real-time structured events emitted by `JimuAgent.prompt(...)`.
public enum JimuEvent: Sendable, Equatable {
    case turnStarted
    case textDelta(contentIndex: Int, delta: String)
    case thinkingDelta(contentIndex: Int, delta: String)
    case toolStarted(toolCallID: String, toolName: String, arguments: JSONValue)
    case toolUpdated(toolCallID: String, toolName: String, arguments: JSONValue, partialResult: JimuToolOutput)
    case toolFinished(toolCallID: String, toolName: String, result: JimuToolOutput, isError: Bool)
    case requestUsage(model: String, provider: String, usage: JimuUsage)
    case steered(id: String, text: String)
    case contextAppended(name: String, text: String)
    case compactionStarted(tokensBefore: UInt64)
    case compactionEnded(summary: String, tokensBefore: UInt64, firstKeptEntryID: String, usage: JimuUsage?)
    case turnEnded(stopReason: JimuStopReason, userMessageKept: Bool, errorMessage: String?)
}

/// Outcome of a manual `compact()` call.
public struct JimuCompactionResult: Codable, Sendable, Equatable {
    public let entryID: String
    public let summary: String
    public let firstKeptEntryID: String
    public let tokensBefore: UInt64
    public let usage: JimuUsage?

    enum CodingKeys: String, CodingKey {
        case entryID = "entry_id"
        case summary
        case firstKeptEntryID = "first_kept_entry_id"
        case tokensBefore = "tokens_before"
        case usage
    }
}

/// One append-only entry stored in the session tree / JSONL journal.
public struct JimuSessionEntry: Sendable, Equatable {
    public let id: String
    public let parentID: String?
    public let timestamp: UInt64
    public let type: String
    public let raw: JSONValue

    /// Whether this entry is hidden from the user-facing transcript.
    public var isMeta: Bool {
        switch type {
        case "context_block", "compaction":
            return true
        case "message":
            let message = raw["message"]
            if message?["kind"]?.stringValue == "custom" {
                return message?["display"]?.boolValue == false
            }
            return message?["is_meta"]?.boolValue == true
        default:
            return false
        }
    }

    init(raw: JSONValue) throws {
        guard case .object(let dict) = raw,
              let id = dict["id"]?.stringValue,
              let type = dict["type"]?.stringValue
        else {
            throw JimuError.internalError("Malformed session entry JSON")
        }
        self.id = id
        self.parentID = dict["parent_id"]?.stringValue
        self.timestamp = UInt64(dict["timestamp"]?.doubleValue ?? 0)
        self.type = type
        self.raw = raw
    }
}

/// Dynamic per-request authorization hook invoked before every outbound LLM HTTP request.
public typealias JimuRequestAuthorizer = @Sendable (inout URLRequest) async throws -> Void

/// Full configuration for opening a `JimuAgent`.
public struct JimuConfiguration: Sendable {
    public var model: JimuModel
    public var systemPrompt: String
    public var tools: [JimuTool]
    public var thinkingLevel: JimuThinkingLevel
    public var fast: Bool
    public var apiKey: String?
    public var temperature: Double?
    public var maxTokens: UInt32?
    public var cacheRetention: JimuCacheRetention
    public var headers: [(String, String?)]
    public var samplingParams: [String: JSONValue]
    public var skillRoots: [URL]
    public var skillDiscovery: JimuSkillDiscovery.Configuration? = nil
    /// Re-scan configured skill roots automatically before each turn (default: `true`).
    public var autoReloadSkills: Bool
    public var instructionDiscovery: JimuInstructionDiscovery.Configuration? = nil
    /// Set a schema budget to defer additional tools when the deferred gateway is enabled.
    public var autoDeferThresholdBytes: UInt? = nil
    /// Receives background events for this environment and shuts it down when the agent closes.
    public var workspaceEnvironment: JimuWorkspaceEnvironment? = nil
    public var enableDeferredGateway: Bool
    public var enableCodemode: Bool
    public var codemodeOptions: JimuCodemodeOptions?
    public var persistenceURL: URL?
    public var cwd: String?
    public var compaction: JimuCompactionConfig
    public var toolExecutionMode: JimuToolExecutionMode
    public var steeringMode: JimuQueueMode
    public var followUpMode: JimuQueueMode
    public var rollbackFirstTurnOnFailure: Bool
    public var jsExtensions: JimuJsExtensionConfig?
    public var httpClient: (any JimuHTTPClient)?
    /// Optional dynamic per-request auth hook invoked right before `beforeRequest` and `httpClient.send`.
    public var authorizeRequest: JimuRequestAuthorizer?
    /// Optional hook invoked right before every LLM HTTP request with the exact JSON request body bytes.
    public var beforeRequest: (@Sendable (Data) async throws -> Void)?

    public init(
        model: JimuModel,
        systemPrompt: String = "",
        tools: [JimuTool] = [],
        thinkingLevel: JimuThinkingLevel = .off,
        fast: Bool = false,
        apiKey: String? = nil,
        temperature: Double? = nil,
        maxTokens: UInt32? = nil,
        cacheRetention: JimuCacheRetention = .short,
        headers: [(String, String?)] = [],
        samplingParams: [String: JSONValue] = [:],
        skillRoots: [URL] = [],
        autoReloadSkills: Bool = true,
        enableDeferredGateway: Bool = true,
        enableCodemode: Bool = false,
        codemodeOptions: JimuCodemodeOptions? = nil,
        persistenceURL: URL? = nil,
        cwd: String? = nil,
        compaction: JimuCompactionConfig = JimuCompactionConfig(),
        toolExecutionMode: JimuToolExecutionMode = .parallel,
        steeringMode: JimuQueueMode = .oneAtATime,
        followUpMode: JimuQueueMode = .oneAtATime,
        rollbackFirstTurnOnFailure: Bool = false,
        jsExtensions: JimuJsExtensionConfig? = nil,
        httpClient: (any JimuHTTPClient)? = nil,
        authorizeRequest: JimuRequestAuthorizer? = nil,
        beforeRequest: (@Sendable (Data) async throws -> Void)? = nil
    ) {
        self.model = model
        self.systemPrompt = systemPrompt
        self.tools = tools
        self.thinkingLevel = thinkingLevel
        self.fast = fast
        self.apiKey = apiKey
        self.temperature = temperature
        self.maxTokens = maxTokens
        self.cacheRetention = cacheRetention
        self.headers = headers
        self.samplingParams = samplingParams
        self.skillRoots = skillRoots
        self.autoReloadSkills = autoReloadSkills
        self.enableDeferredGateway = enableDeferredGateway
        self.enableCodemode = enableCodemode
        self.codemodeOptions = codemodeOptions
        self.persistenceURL = persistenceURL
        self.cwd = cwd
        self.compaction = compaction
        self.toolExecutionMode = toolExecutionMode
        self.steeringMode = steeringMode
        self.followUpMode = followUpMode
        self.rollbackFirstTurnOnFailure = rollbackFirstTurnOnFailure
        self.jsExtensions = jsExtensions
        self.httpClient = httpClient
        self.authorizeRequest = authorizeRequest
        self.beforeRequest = beforeRequest
    }
}
