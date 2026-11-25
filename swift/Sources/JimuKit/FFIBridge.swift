import CJimu
import Foundation

// MARK: - Wire DTOs

struct WireToolSpec: Encodable {
    let name: String
    let description: String
    let parameters: JSONValue
    let deferred: Bool
    let executionMode: JimuToolExecutionMode

    enum CodingKeys: String, CodingKey {
        case name
        case description
        case parameters
        case deferred
        case executionMode = "execution_mode"
    }

    init(tool: JimuTool, forceDeferred: Bool? = nil) {
        self.name = tool.name
        self.description = tool.description
        self.parameters = tool.parameters
        self.deferred = forceDeferred ?? tool.deferred
        self.executionMode = tool.executionMode
    }
}

struct WireJsExtensionSource: Encodable {
    let type: String
    let name: String?
    let code: String?
    let path: String?

    init(_ source: JimuJsExtensionSource) {
        switch source {
        case .inline(let name, let code):
            self.type = "inline"
            self.name = name
            self.code = code
            self.path = nil
        case .file(let url):
            self.type = "file"
            self.name = nil
            self.code = nil
            self.path = url.path
        }
    }
}

struct WireJsExtensionConfig: Encodable {
    let sources: [WireJsExtensionSource]
    let memoryLimitBytes: Int
    let maxStackSizeBytes: Int
    let timeoutMs: UInt64

    enum CodingKeys: String, CodingKey {
        case sources
        case memoryLimitBytes = "memory_limit_bytes"
        case maxStackSizeBytes = "max_stack_size_bytes"
        case timeoutMs = "timeout_ms"
    }

    init(_ cfg: JimuJsExtensionConfig) {
        self.sources = cfg.sources.map(WireJsExtensionSource.init)
        self.memoryLimitBytes = cfg.memoryLimitBytes
        self.maxStackSizeBytes = cfg.maxStackSizeBytes
        self.timeoutMs = cfg.timeoutMs
    }
}

struct WireHeaderOverride: Encodable {
    let name: String
    let value: String?

    func encode(to encoder: any Encoder) throws {
        var container = encoder.unkeyedContainer()
        try container.encode(name)
        if let value {
            try container.encode(value)
        } else {
            try container.encodeNil()
        }
    }
}

struct WireSessionConfig: Encodable {
    let model: JimuModel
    let systemPrompt: String
    let tools: [WireToolSpec]
    let thinkingLevel: JimuThinkingLevel
    let fast: Bool
    let apiKey: String?
    let temperature: Double?
    let maxTokens: UInt32?
    let cacheRetention: JimuCacheRetention
    let headers: [WireHeaderOverride]
    let samplingParams: [String: JSONValue]
    let skillRoots: [String]
    let skillDiscovery: JimuSkillDiscovery.Configuration?
    let autoReloadSkills: Bool
    let instructionDiscovery: JimuInstructionDiscovery.Configuration?
    let autoDeferThresholdBytes: UInt?
    let enableDeferredGateway: Bool
    let enableCodemode: Bool
    let codemodeOptions: JimuCodemodeOptions?
    let persistencePath: String?
    let cwd: String?
    let compaction: JimuCompactionConfig
    let toolExecutionMode: JimuToolExecutionMode
    let steeringMode: JimuQueueMode
    let followUpMode: JimuQueueMode
    let rollbackFirstTurnOnFailure: Bool
    let jsExtensions: WireJsExtensionConfig?

    enum CodingKeys: String, CodingKey {
        case model
        case systemPrompt = "system_prompt"
        case tools
        case thinkingLevel = "thinking_level"
        case fast
        case apiKey = "api_key"
        case temperature
        case maxTokens = "max_tokens"
        case cacheRetention = "cache_retention"
        case headers
        case samplingParams = "sampling_params"
        case skillRoots = "skill_roots"
        case skillDiscovery = "skill_discovery"
        case autoReloadSkills = "auto_reload_skills"
        case instructionDiscovery = "instruction_discovery"
        case autoDeferThresholdBytes = "auto_defer_threshold_bytes"
        case enableDeferredGateway = "enable_deferred_gateway"
        case enableCodemode = "enable_codemode"
        case codemodeOptions = "codemode_options"
        case persistencePath = "persistence_path"
        case cwd
        case compaction
        case toolExecutionMode = "tool_execution_mode"
        case steeringMode = "steering_mode"
        case followUpMode = "follow_up_mode"
        case rollbackFirstTurnOnFailure = "rollback_first_turn_on_failure"
        case jsExtensions = "js_extensions"
    }

    init(_ config: JimuConfiguration) {
        self.model = config.model
        self.systemPrompt = config.systemPrompt
        self.tools = config.tools.map { WireToolSpec(tool: $0) }
        self.thinkingLevel = config.thinkingLevel
        self.fast = config.fast
        self.apiKey = config.apiKey
        self.temperature = config.temperature
        self.maxTokens = config.maxTokens
        self.cacheRetention = config.cacheRetention
        self.headers = config.headers.map { WireHeaderOverride(name: $0.0, value: $0.1) }
        self.samplingParams = config.samplingParams
        self.skillRoots = config.skillRoots.map(\.path)
        self.skillDiscovery = config.skillDiscovery
        self.autoReloadSkills = config.autoReloadSkills
        self.instructionDiscovery = config.instructionDiscovery
        self.autoDeferThresholdBytes = config.autoDeferThresholdBytes
        self.enableDeferredGateway = config.enableDeferredGateway
        self.enableCodemode = config.enableCodemode
        self.codemodeOptions = config.codemodeOptions
        self.persistencePath = config.persistenceURL?.path
        self.cwd = config.cwd
        self.compaction = config.compaction
        self.toolExecutionMode = config.toolExecutionMode
        self.steeringMode = config.steeringMode
        self.followUpMode = config.followUpMode
        self.rollbackFirstTurnOnFailure = config.rollbackFirstTurnOnFailure
        self.jsExtensions = config.jsExtensions.map(WireJsExtensionConfig.init)
    }
}

struct WirePromptRequest: Encodable {
    let id: String?
    let content: [JimuContentPart]
    let isMeta: Bool

    enum CodingKeys: String, CodingKey {
        case id
        case content
        case isMeta = "is_meta"
    }
}

struct WireHttpRequestHead: Decodable {
    let method: String
    let url: String
    let headers: [[String]]
    let timeoutMs: UInt64?

    enum CodingKeys: String, CodingKey {
        case method
        case url
        case headers
        case timeoutMs = "timeout_ms"
    }
}

struct WireSessionInfo: Decodable {
    let sessionID: String
    let leafID: String?
    let model: JimuModel
    let thinkingLevel: JimuThinkingLevel
    let fast: Bool
    let isRunning: Bool
    let residentTools: [Tool]

    struct Tool: Decodable { let name: String }

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case leafID = "leaf_id"
        case model
        case thinkingLevel = "thinking_level"
        case fast
        case isRunning = "is_running"
        case residentTools = "resident_tools"
    }
}

struct WireEvent: Decodable {
    let type: String
    let contentIndex: Int?
    let delta: String?
    let toolCallID: String?
    let toolName: String?
    let args: JSONValue?
    let partialResult: JimuToolOutput?
    let result: JimuToolOutput?
    let isError: Bool?
    let model: String?
    let provider: String?
    let usage: JimuUsage?
    let id: String?
    let text: String?
    let name: String?
    let tokensBefore: UInt64?
    let summary: String?
    let firstKeptEntryID: String?
    let stopReason: JimuStopReason?
    let userMessageKept: Bool?
    let errorMessage: String?

    enum CodingKeys: String, CodingKey {
        case type
        case contentIndex = "content_index"
        case delta
        case toolCallID = "tool_call_id"
        case toolName = "tool_name"
        case args
        case partialResult = "partial_result"
        case result
        case isError = "is_error"
        case model
        case provider
        case usage
        case id
        case text
        case name
        case tokensBefore = "tokens_before"
        case summary
        case firstKeptEntryID = "first_kept_entry_id"
        case stopReason = "stop_reason"
        case userMessageKept = "user_message_kept"
        case errorMessage = "error_message"
    }

    func toEvent() -> JimuEvent? {
        switch type {
        case "turn_started":
            return .turnStarted
        case "text_delta":
            return .textDelta(contentIndex: contentIndex ?? 0, delta: delta ?? "")
        case "thinking_delta":
            return .thinkingDelta(contentIndex: contentIndex ?? 0, delta: delta ?? "")
        case "tool_started":
            return .toolStarted(
                toolCallID: toolCallID ?? "",
                toolName: toolName ?? "",
                arguments: args ?? .object([:])
            )
        case "tool_updated":
            guard let partialResult else { return nil }
            return .toolUpdated(
                toolCallID: toolCallID ?? "",
                toolName: toolName ?? "",
                arguments: args ?? .object([:]),
                partialResult: partialResult
            )
        case "tool_finished":
            guard let result else { return nil }
            return .toolFinished(
                toolCallID: toolCallID ?? "",
                toolName: toolName ?? "",
                result: result,
                isError: isError ?? result.isError
            )
        case "request_usage":
            guard let usage else { return nil }
            return .requestUsage(model: model ?? "", provider: provider ?? "", usage: usage)
        case "steered":
            return .steered(id: id ?? "", text: text ?? "")
        case "context_appended":
            return .contextAppended(name: name ?? "", text: text ?? "")
        case "compaction_started":
            return .compactionStarted(tokensBefore: tokensBefore ?? 0)
        case "compaction_ended":
            return .compactionEnded(
                summary: summary ?? "",
                tokensBefore: tokensBefore ?? 0,
                firstKeptEntryID: firstKeptEntryID ?? "",
                usage: usage
            )
        case "turn_ended":
            return .turnEnded(
                stopReason: stopReason ?? .stop,
                userMessageKept: userMessageKept ?? true,
                errorMessage: errorMessage
            )
        default:
            return nil
        }
    }
}

// MARK: - Pointer Sendable Wrappers

struct SendableOpaquePointer: @unchecked Sendable {
    let raw: UnsafeMutableRawPointer
}

struct SendableHttpStreamPtr: @unchecked Sendable {
    let raw: OpaquePointer
}

struct SendableToolCompletionPtr: @unchecked Sendable {
    let raw: OpaquePointer
}

// MARK: - Status Mapping Helpers

func mapStatusError(_ status: JimuStatus, message: String?) -> JimuError {
    let msg = message ?? "status \(status.rawValue)"
    switch status {
    case JIMU_ERR_INVALID_ARGUMENT:
        return .invalidArgument(msg)
    case JIMU_ERR_BUSY:
        return .busy
    case JIMU_ERR_CLOSED:
        return .closed
    case JIMU_ERR_NOT_FOUND:
        return .notFound(msg)
    case JIMU_ERR_IO:
        return .io(msg)
    case JIMU_ERR_CANCELLED:
        return .cancelled
    default:
        return .internalError(msg)
    }
}

func takeOwnedCString(_ ptr: UnsafeMutablePointer<CChar>?) -> String? {
    guard let ptr else { return nil }
    defer { jimu_string_free(ptr) }
    return String(cString: ptr)
}

func encodeJSONString<T: Encodable>(_ value: T) throws -> String {
    let data = try JSONEncoder().encode(value)
    guard let str = String(data: data, encoding: .utf8) else {
        throw JimuError.internalError("UTF-8 JSON encoding failed")
    }
    return str
}

// MARK: - Session Bridge (HTTP + Host Tools)

final class JimuSessionBridge: @unchecked Sendable {
    private let httpClient: any JimuHTTPClient
    private let authorizeRequest: JimuRequestAuthorizer?
    private let beforeRequest: (@Sendable (Data) async throws -> Void)?

    private let lock = NSLock()
    private var toolsByName: [String: JimuTool]
    private var httpTasks: [UInt64: Task<Void, Never>] = [:]
    private var toolTasks: [UInt64: Task<Void, Never>] = [:]
    private var cancelledToolHandles: Set<UInt64> = []

    init(
        httpClient: any JimuHTTPClient,
        authorizeRequest: JimuRequestAuthorizer?,
        beforeRequest: (@Sendable (Data) async throws -> Void)?,
        initialTools: [JimuTool]
    ) {
        self.httpClient = httpClient
        self.authorizeRequest = authorizeRequest
        self.beforeRequest = beforeRequest
        self.toolsByName = Dictionary(initialTools.map { ($0.name, $0) }, uniquingKeysWith: { _, last in last })
    }

    func upsertTools(_ tools: [JimuTool]) {
        lock.withLock {
            for tool in tools {
                toolsByName[tool.name] = tool
            }
        }
    }

    func removeTools(named names: [String]) {
        lock.withLock {
            for name in names {
                toolsByName.removeValue(forKey: name)
            }
        }
    }

    func cancelAll() {
        let (httpList, toolList) = lock.withLock { () -> ([Task<Void, Never>], [Task<Void, Never>]) in
            let h = Array(httpTasks.values)
            let t = Array(toolTasks.values)
            httpTasks.removeAll()
            toolTasks.removeAll()
            cancelledToolHandles.removeAll()
            return (h, t)
        }
        for task in httpList { task.cancel() }
        for task in toolList { task.cancel() }
    }

    // MARK: HTTP Dispatch

    func startHTTP(
        requestID: UInt64,
        requestJSON: String,
        body: Data,
        streamPtr: SendableHttpStreamPtr
    ) {
        lock.withLock {
            httpTasks[requestID] = Task { [weak self] in
                defer { self?.removeHTTPTask(requestID) }
                guard let self else {
                    jimu_http_finish(streamPtr.raw, Int32(JIMU_HTTP_ERR_ABORTED.rawValue), "session destroyed")
                    return
                }
                await self.driveHTTP(
                    requestID: requestID,
                    requestJSON: requestJSON,
                    body: body,
                    streamPtr: streamPtr
                )
            }
        }
    }

    func cancelHTTP(requestID: UInt64) {
        let task = lock.withLock { httpTasks.removeValue(forKey: requestID) }
        task?.cancel()
    }

    private func removeHTTPTask(_ requestID: UInt64) {
        lock.withLock {
            _ = httpTasks.removeValue(forKey: requestID)
        }
    }

    private func driveHTTP(
        requestID: UInt64,
        requestJSON: String,
        body: Data,
        streamPtr: SendableHttpStreamPtr
    ) async {
        var finished = false
        func finish(_ kind: JimuHttpErrorKind, _ message: String? = nil) {
            guard !finished else { return }
            finished = true
            if let message {
                message.withCString { cstr in
                    jimu_http_finish(streamPtr.raw, Int32(kind.rawValue), cstr)
                }
            } else {
                jimu_http_finish(streamPtr.raw, Int32(kind.rawValue), nil)
            }
        }

        do {
            try Task.checkCancellation()
            guard let jsonData = requestJSON.data(using: .utf8) else {
                finish(JIMU_HTTP_ERR_NETWORK, "Invalid request JSON encoding")
                return
            }
            let head = try JSONDecoder().decode(WireHttpRequestHead.self, from: jsonData)
            guard let url = URL(string: head.url) else {
                finish(JIMU_HTTP_ERR_NETWORK, "Invalid request URL: \(head.url)")
                return
            }
            let headers: [(String, String)] = head.headers.compactMap { pair in
                guard pair.count == 2 else { return nil }
                return (pair[0], pair[1])
            }
            let timeout = head.timeoutMs.map { TimeInterval($0) / 1000.0 }
            var request = JimuHTTPRequest(
                method: head.method,
                url: url,
                headers: headers,
                body: body,
                timeoutInterval: timeout
            )

            if let authorizeRequest {
                var urlRequest = request.toURLRequest()
                let originalTimeout = urlRequest.timeoutInterval
                try await authorizeRequest(&urlRequest)
                try Task.checkCancellation()
                request = JimuHTTPRequest(
                    applying: urlRequest,
                    fallback: request,
                    originalTimeout: originalTimeout
                )
            }

            if let beforeRequest {
                try await beforeRequest(request.body)
            }
            try Task.checkCancellation()

            let response = try await httpClient.send(request)
            if Task.isCancelled {
                response.cancel()
                finish(JIMU_HTTP_ERR_ABORTED, "cancelled")
                return
            }

            let headersArray = response.headers.map { [$0.0, $0.1] }
            let headersJSON = try encodeJSONString(headersArray)
            let headStatus = headersJSON.withCString { cstr in
                jimu_http_on_response(streamPtr.raw, UInt16(clamping: response.statusCode), cstr)
            }
            if headStatus != JIMU_OK {
                response.cancel()
                finish(JIMU_HTTP_ERR_ABORTED, "cancelled")
                return
            }

            try await withTaskCancellationHandler {
                for try await chunk in response.body {
                    try Task.checkCancellation()
                    guard !chunk.isEmpty else { continue }
                    while true {
                        let pushStatus = chunk.withUnsafeBytes { rawBuf -> JimuStatus in
                            let ptr = rawBuf.bindMemory(to: UInt8.self).baseAddress
                            return jimu_http_push_chunk(streamPtr.raw, ptr, rawBuf.count)
                        }
                        if pushStatus == JIMU_OK {
                            break
                        } else if pushStatus == JIMU_ERR_BACKPRESSURE {
                            try Task.checkCancellation()
                            await Task.yield()
                            continue
                        } else {
                            response.cancel()
                            finish(JIMU_HTTP_ERR_ABORTED, "cancelled")
                            return
                        }
                    }
                }
            } onCancel: {
                response.cancel()
            }

            if Task.isCancelled {
                finish(JIMU_HTTP_ERR_ABORTED, "cancelled")
            } else {
                finish(JIMU_HTTP_OK, nil)
            }
        } catch is CancellationError {
            finish(JIMU_HTTP_ERR_ABORTED, "cancelled")
        } catch let urlErr as URLError where urlErr.code == .cancelled {
            finish(JIMU_HTTP_ERR_ABORTED, "cancelled")
        } catch let urlErr as URLError where urlErr.code == .timedOut {
            finish(JIMU_HTTP_ERR_TIMEOUT, urlErr.localizedDescription)
        } catch {
            finish(JIMU_HTTP_ERR_NETWORK, String(describing: error))
        }
    }

    // MARK: Host Tool Dispatch

    func startTool(
        callHandleID: UInt64,
        toolCallID: String,
        toolName: String,
        argsJSON: String,
        completionPtr: SendableToolCompletionPtr
    ) {
        let alreadyCancelled = lock.withLock { () -> Bool in
            if cancelledToolHandles.remove(callHandleID) != nil {
                return true
            }
            let task = Task { [weak self] in
                defer {
                    self?.removeToolTask(callHandleID)
                }
                guard let self else {
                    "session destroyed".withCString { cstr in
                        jimu_tool_complete(completionPtr.raw, nil, cstr)
                    }
                    return
                }
                await self.driveTool(
                    toolCallID: toolCallID,
                    toolName: toolName,
                    argsJSON: argsJSON,
                    completionPtr: completionPtr
                )
            }
            toolTasks[callHandleID] = task
            return false
        }
        if alreadyCancelled {
            "cancelled".withCString { cstr in
                jimu_tool_complete(completionPtr.raw, nil, cstr)
            }
        }
    }

    func cancelTool(callHandleID: UInt64) {
        let task = lock.withLock { () -> Task<Void, Never>? in
            if let existing = toolTasks.removeValue(forKey: callHandleID) {
                return existing
            }
            cancelledToolHandles.insert(callHandleID)
            return nil
        }
        task?.cancel()
    }

    private func removeToolTask(_ callHandleID: UInt64) {
        lock.withLock {
            _ = toolTasks.removeValue(forKey: callHandleID)
            _ = cancelledToolHandles.remove(callHandleID)
        }
    }

    private final class ToolCompletionGuard: @unchecked Sendable {
        private let lock = NSLock()
        private let ptr: OpaquePointer
        private var completed = false

        init(_ ptr: OpaquePointer) {
            self.ptr = ptr
        }

        func reportProgress(_ partial: JimuToolOutput) {
            lock.withLock {
                guard !completed else { return }
                if let json = try? encodeJSONString(partial) {
                    json.withCString { cstr in
                        _ = jimu_tool_update(ptr, cstr)
                    }
                }
            }
        }

        func complete(resultJSON: String?, errorMessage: String?) {
            let shouldComplete = lock.withLock { () -> Bool in
                if completed { return false }
                completed = true
                return true
            }
            guard shouldComplete else { return }
            if let errorMessage {
                errorMessage.withCString { errC in
                    jimu_tool_complete(ptr, nil, errC)
                }
            } else if let resultJSON {
                resultJSON.withCString { resC in
                    jimu_tool_complete(ptr, resC, nil)
                }
            } else {
                "empty tool result".withCString { errC in
                    jimu_tool_complete(ptr, nil, errC)
                }
            }
        }
    }

    private func driveTool(
        toolCallID: String,
        toolName: String,
        argsJSON: String,
        completionPtr: SendableToolCompletionPtr
    ) async {
        let guardBox = ToolCompletionGuard(completionPtr.raw)

        let tool = lock.withLock { toolsByName[toolName] }
        guard let tool else {
            guardBox.complete(resultJSON: nil, errorMessage: "Tool not registered on Swift host: \(toolName)")
            return
        }

        let parsedArgs: JSONValue
        if let data = argsJSON.data(using: .utf8),
           let decoded = try? JSONDecoder().decode(JSONValue.self, from: data)
        {
            parsedArgs = decoded
        } else {
            parsedArgs = .object([:])
        }

        let context = JimuToolCallContext(
            toolCallID: toolCallID,
            toolName: toolName
        ) { partial in
            guardBox.reportProgress(partial)
        }

        do {
            try Task.checkCancellation()
            let output = try await tool.execute(context, parsedArgs)
            let json = try encodeJSONString(output)
            guardBox.complete(resultJSON: json, errorMessage: nil)
        } catch is CancellationError {
            guardBox.complete(resultJSON: nil, errorMessage: "cancelled")
        } catch {
            guardBox.complete(resultJSON: nil, errorMessage: error.localizedDescription)
        }
    }
}

// MARK: - C Trampolines

let jimuHttpStartTrampoline: JimuHttpStartFn = { ctx, requestID, requestJSONPtr, bodyPtr, bodyLen, streamPtr in
    guard let ctx, let streamPtr else { return }
    let bridge = Unmanaged<JimuSessionBridge>.fromOpaque(ctx).takeUnretainedValue()
    let requestJSON = requestJSONPtr.map { String(cString: $0) } ?? "{}"
    let body: Data
    if let bodyPtr, bodyLen > 0 {
        body = Data(bytes: bodyPtr, count: bodyLen)
    } else {
        body = Data()
    }
    bridge.startHTTP(
        requestID: requestID,
        requestJSON: requestJSON,
        body: body,
        streamPtr: SendableHttpStreamPtr(raw: streamPtr)
    )
}

let jimuHttpCancelTrampoline: JimuHttpCancelFn = { ctx, requestID in
    guard let ctx else { return }
    let bridge = Unmanaged<JimuSessionBridge>.fromOpaque(ctx).takeUnretainedValue()
    bridge.cancelHTTP(requestID: requestID)
}

let jimuBridgeDestroyTrampoline: JimuVoidDoneFn = { ctx in
    guard let ctx else { return }
    let bridge = Unmanaged<JimuSessionBridge>.fromOpaque(ctx).takeRetainedValue()
    bridge.cancelAll()
}

let jimuToolExecuteTrampoline: JimuToolExecuteFn = { ctx, callHandleID, toolCallIDPtr, toolNamePtr, argsJSONPtr, completionPtr in
    guard let ctx, let completionPtr else { return }
    let bridge = Unmanaged<JimuSessionBridge>.fromOpaque(ctx).takeUnretainedValue()
    let toolCallID = toolCallIDPtr.map { String(cString: $0) } ?? ""
    let toolName = toolNamePtr.map { String(cString: $0) } ?? ""
    let argsJSON = argsJSONPtr.map { String(cString: $0) } ?? "{}"
    bridge.startTool(
        callHandleID: callHandleID,
        toolCallID: toolCallID,
        toolName: toolName,
        argsJSON: argsJSON,
        completionPtr: SendableToolCompletionPtr(raw: completionPtr)
    )
}

let jimuToolCancelTrampoline: JimuToolCancelFn = { ctx, callHandleID in
    guard let ctx else { return }
    let bridge = Unmanaged<JimuSessionBridge>.fromOpaque(ctx).takeUnretainedValue()
    bridge.cancelTool(callHandleID: callHandleID)
}

// MARK: - Async Continuation Boxes

final class PromptStreamBox: @unchecked Sendable {
    let continuation: AsyncThrowingStream<JimuEvent, any Error>.Continuation
    private let decoder = JSONDecoder()

    init(continuation: AsyncThrowingStream<JimuEvent, any Error>.Continuation) {
        self.continuation = continuation
    }

    func onEvent(json: String) {
        guard let data = json.data(using: .utf8),
              let wire = try? decoder.decode(WireEvent.self, from: data),
              let event = wire.toEvent()
        else {
            return
        }
        continuation.yield(event)
    }

    func onDone(status: JimuStatus, errorMessage: String?) {
        if status == JIMU_OK {
            continuation.finish()
        } else {
            continuation.finish(throwing: mapStatusError(status, message: errorMessage))
        }
    }
}

let jimuPromptEventTrampoline: JimuEventFn = { ctx, eventJSONPtr in
    guard let ctx, let eventJSONPtr else { return }
    let box = Unmanaged<PromptStreamBox>.fromOpaque(ctx).takeUnretainedValue()
    box.onEvent(json: String(cString: eventJSONPtr))
}

let jimuPromptDoneTrampoline: JimuStreamDoneFn = { ctx, status, errorPtr in
    guard let ctx else { return }
    let box = Unmanaged<PromptStreamBox>.fromOpaque(ctx).takeRetainedValue()
    let errMsg = errorPtr.map { String(cString: $0) }
    box.onDone(status: status, errorMessage: errMsg)
}

final class VoidContinuationBox: @unchecked Sendable {
    let continuation: CheckedContinuation<Void, Never>

    init(_ continuation: CheckedContinuation<Void, Never>) {
        self.continuation = continuation
    }
}

let jimuVoidDoneTrampoline: JimuVoidDoneFn = { ctx in
    guard let ctx else { return }
    let box = Unmanaged<VoidContinuationBox>.fromOpaque(ctx).takeRetainedValue()
    box.continuation.resume()
}

final class CompactionContinuationBox: @unchecked Sendable {
    let continuation: CheckedContinuation<JimuCompactionResult?, any Error>

    init(_ continuation: CheckedContinuation<JimuCompactionResult?, any Error>) {
        self.continuation = continuation
    }
}

let jimuCompactDoneTrampoline: JimuResultDoneFn = { ctx, status, resultJSONPtr, errorPtr in
    guard let ctx else { return }
    let box = Unmanaged<CompactionContinuationBox>.fromOpaque(ctx).takeRetainedValue()
    if status != JIMU_OK {
        let errMsg = errorPtr.map { String(cString: $0) }
        box.continuation.resume(throwing: mapStatusError(status, message: errMsg))
        return
    }
    guard let resultJSONPtr else {
        box.continuation.resume(returning: nil)
        return
    }
    let jsonStr = String(cString: resultJSONPtr)
    guard let data = jsonStr.data(using: .utf8) else {
        box.continuation.resume(throwing: JimuError.internalError("Invalid compaction JSON encoding"))
        return
    }
    do {
        let decoded = try JSONDecoder().decode(JimuCompactionResult.self, from: data)
        box.continuation.resume(returning: decoded)
    } catch {
        box.continuation.resume(throwing: JimuError.internalError("Decode compaction result: \(error)"))
    }
}
