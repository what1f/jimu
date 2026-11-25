import CJimu
import Foundation

/// Swift 6 strict-concurrency actor driving a Jimu headless agent session over C-ABI (`CJimu`).
public actor JimuAgent {
    private final class HandleBox: @unchecked Sendable {
        var session: OpaquePointer?
        let bridge: JimuSessionBridge

        init(session: OpaquePointer, bridge: JimuSessionBridge) {
            self.session = session
            self.bridge = bridge
        }

        func destroyNow() {
            bridge.cancelAll()
            if let s = session {
                session = nil
                // Cancels any active Rust run and drops Swift's Arc<FfiSession>.
                // The HTTP callbacks retain the bridge until the last Rust request releases it.
                jimu_session_destroy(s)
            }
        }

        deinit {
            destroyNow()
        }
    }

    private let handleBox: HandleBox
    private let bridge: JimuSessionBridge
    private var isClosed = false
    private let workspaceEnvironments: [JimuWorkspaceEnvironment]
    private let backgroundForwarder: Task<Void, Never>?
    /// Background events from workspace environments configured directly or attached to built-in tools.
    public nonisolated let backgroundEvents: AsyncStream<JimuBackgroundEvent>

    private init(
        session: OpaquePointer,
        bridge: JimuSessionBridge,
        workspaceEnvironments: [JimuWorkspaceEnvironment]
    ) {
        self.handleBox = HandleBox(session: session, bridge: bridge)
        self.bridge = bridge
        self.workspaceEnvironments = workspaceEnvironments
        if workspaceEnvironments.isEmpty {
            self.backgroundEvents = AsyncStream { $0.finish() }
            self.backgroundForwarder = nil
        } else if workspaceEnvironments.count == 1 {
            self.backgroundEvents = workspaceEnvironments[0].backgroundEvents
            self.backgroundForwarder = nil
        } else {
            let (stream, continuation) = AsyncStream<JimuBackgroundEvent>.makeStream(
                bufferingPolicy: .bufferingNewest(256)
            )
            self.backgroundEvents = stream
            self.backgroundForwarder = Task {
                await withTaskGroup(of: Void.self) { group in
                    for environment in workspaceEnvironments {
                        group.addTask {
                            for await event in environment.backgroundEvents {
                                continuation.yield(event)
                            }
                        }
                    }
                    await group.waitForAll()
                }
                continuation.finish()
            }
        }
    }

    deinit {
        backgroundForwarder?.cancel()
        for environment in workspaceEnvironments {
            environment.beginShutdown()
        }
    }

    /// Opens a new or existing `JimuAgent` session (in-memory or JSONL-persisted).
    public static func open(configuration: JimuConfiguration) async throws -> JimuAgent {
        try Task.checkCancellation()

        let httpClient = configuration.httpClient ?? JimuURLSessionHTTPClient()
        let bridge = JimuSessionBridge(
            httpClient: httpClient,
            authorizeRequest: configuration.authorizeRequest,
            beforeRequest: configuration.beforeRequest,
            initialTools: configuration.tools
        )
        let bridgePtr = Unmanaged.passRetained(bridge).toOpaque()

        let httpCallbacks = JimuHttpCallbacks(
            ctx: bridgePtr,
            start: jimuHttpStartTrampoline,
            cancel: jimuHttpCancelTrampoline,
            destroy: jimuBridgeDestroyTrampoline
        )
        let toolCallbacks = JimuToolCallbacks(
            ctx: bridgePtr,
            execute: jimuToolExecuteTrampoline,
            cancel: jimuToolCancelTrampoline
        )

        let configJSON: String
        do {
            configJSON = try encodeJSONString(WireSessionConfig(configuration))
        } catch {
            Unmanaged<JimuSessionBridge>.fromOpaque(bridgePtr).release()
            throw error
        }

        var errorPtr: UnsafeMutablePointer<CChar>?
        let sessionPtr = configJSON.withCString { cstr in
            jimu_session_create(cstr, httpCallbacks, toolCallbacks, &errorPtr)
        }
        guard let sessionPtr else {
            Unmanaged<JimuSessionBridge>.fromOpaque(bridgePtr).release()
            let message = takeOwnedCString(errorPtr) ?? "Failed to create JimuSession"
            throw JimuError.invalidArgument(message)
        }

        var seenEnvironments: Set<ObjectIdentifier> = []
        var workspaceEnvironments: [JimuWorkspaceEnvironment] = []
        let appendEnvironment: (JimuWorkspaceEnvironment?) -> Void = { environment in
            guard let environment else { return }
            let id = ObjectIdentifier(environment)
            if seenEnvironments.insert(id).inserted {
                workspaceEnvironments.append(environment)
            }
        }
        appendEnvironment(configuration.workspaceEnvironment)
        for tool in configuration.tools {
            appendEnvironment(tool.associatedEnvironment)
        }

        return JimuAgent(
            session: sessionPtr,
            bridge: bridge,
            workspaceEnvironments: workspaceEnvironments
        )
    }

    private func requireSession() throws -> OpaquePointer {
        guard !isClosed, let session = handleBox.session else {
            throw JimuError.closed
        }
        return session
    }

    // MARK: - Prompting & Streaming

    /// Sends a text prompt and returns a real-time `AsyncThrowingStream` of `JimuEvent`s.
    public func prompt(
        _ text: String,
        id: String? = nil,
        isMeta: Bool = false
    ) async throws -> AsyncThrowingStream<JimuEvent, any Error> {
        try await prompt([JimuContentPart.text(text)], id: id, isMeta: isMeta)
    }

    /// Sends a multimodal prompt (text and/or images) and returns a real-time stream of `JimuEvent`s.
    public func prompt(
        _ content: [JimuPromptContent],
        id: String? = nil,
        isMeta: Bool = false
    ) async throws -> AsyncThrowingStream<JimuEvent, any Error> {
        try await prompt(content.map { $0.toWirePart() }, id: id, isMeta: isMeta)
    }

    /// Sends content parts already encoded for transport. Prefer the existing prompt content
    /// overload for untyped array literals, which may include images supplied as Data.
    @_disfavoredOverload
    public func prompt(
        _ content: [JimuContentPart],
        id: String? = nil,
        isMeta: Bool = false
    ) async throws -> AsyncThrowingStream<JimuEvent, any Error> {
        let session = try requireSession()
        guard !content.isEmpty else {
            throw JimuError.invalidArgument("Prompt content must not be empty")
        }
        let wirePrompt = WirePromptRequest(id: id, content: content, isMeta: isMeta)
        let promptJSON = try encodeJSONString(wirePrompt)

        var capturedContinuation: AsyncThrowingStream<JimuEvent, any Error>.Continuation?
        let stream = AsyncThrowingStream<JimuEvent, any Error> { continuation in
            capturedContinuation = continuation
        }
        guard let continuation = capturedContinuation else {
            throw JimuError.internalError("Failed to initialize event stream")
        }

        let box = PromptStreamBox(continuation: continuation)
        let boxPtr = Unmanaged.passRetained(box).toOpaque()

        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = promptJSON.withCString { cstr in
            jimu_session_prompt_start(
                session,
                cstr,
                boxPtr,
                jimuPromptEventTrampoline,
                jimuPromptDoneTrampoline,
                &errorPtr
            )
        }
        if status != JIMU_OK {
            Unmanaged<PromptStreamBox>.fromOpaque(boxPtr).release()
            let err = mapStatusError(status, message: takeOwnedCString(errorPtr))
            continuation.finish(throwing: err)
            throw err
        }

        return stream
    }

    // MARK: - Mid-Turn Control

    /// Queues a mid-turn steering message. Returns `true` if accepted by an active turn, `false` if idle.
    @discardableResult
    public func steer(text: String, id: String = UUID().uuidString, isMeta: Bool = false) -> Bool {
        guard !isClosed, let session = handleBox.session else { return false }
        return id.withCString { idC in
            text.withCString { textC in
                jimu_session_steer(session, idC, textC, isMeta)
            }
        }
    }

    /// Queues a follow-up user message to execute after the current turn settles.
    public func followUp(text: String, id: String = UUID().uuidString, isMeta: Bool = false) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = id.withCString { idC in
            text.withCString { textC in
                jimu_session_follow_up(session, idC, textC, isMeta, &errorPtr)
            }
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Aborts any active turn, awaits full in-flight tool teardown & unread steer cleanup,
    /// and returns only after the session is guaranteed idle (immediate next call never throws `.busy`).
    public func interrupt() async {
        guard !isClosed, let session = handleBox.session else { return }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            let box = VoidContinuationBox(continuation)
            let boxPtr = Unmanaged.passRetained(box).toOpaque()
            jimu_session_interrupt(session, boxPtr, jimuVoidDoneTrampoline)
        }
    }

    // MARK: - Context & Model Configuration

    /// Sets or updates a named `<system-reminder>` context block (deduplicated; NO-OP if unchanged).
    @discardableResult
    public func setContext(name: String, text: String) throws -> Bool {
        let session = try requireSession()
        var changed = false
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = name.withCString { nameC in
            text.withCString { textC in
                jimu_session_set_context(session, nameC, textC, &changed, &errorPtr)
            }
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        return changed
    }

    /// Queues a one-off `<system-reminder>` context block for the next turn.
    public func appendContext(_ text: String, name: String = "context") throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = name.withCString { nameC in
            text.withCString { textC in
                jimu_session_append_named_context(session, nameC, textC, &errorPtr)
            }
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Queues a hidden user message for the next turn.
    public func appendMeta(_ text: String) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = text.withCString { textC in
            jimu_session_append_meta(session, textC, &errorPtr)
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Switches the active model while idle and appends a `model_change` entry.
    public func setModel(_ model: JimuModel) throws {
        let session = try requireSession()
        let json = try encodeJSONString(model)
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = json.withCString { cstr in
            jimu_session_set_model(session, cstr, &errorPtr)
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Updates reasoning / thinking effort while idle.
    public func setReasoningEffort(_ level: JimuThinkingLevel) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = level.rawValue.withCString { cstr in
            jimu_session_set_thinking_level(session, cstr, &errorPtr)
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Toggles fast mode while idle.
    public func setFast(_ fast: Bool) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = jimu_session_set_fast(session, fast, &errorPtr)
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Updates or clears the active API key while idle (for token rotation without reopening).
    public func setAPIKey(_ apiKey: String?) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status: JimuStatus
        if let apiKey {
            status = apiKey.withCString { cstr in
                jimu_session_set_api_key(session, cstr, &errorPtr)
            }
        } else {
            status = jimu_session_set_api_key(session, nil, &errorPtr)
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    // MARK: - Dynamic Deferred Tools

    /// Adds or replaces deferred tools while idle without mutating the resident `tools` array.
    public func upsertTools(_ tools: [JimuTool]) throws {
        let session = try requireSession()
        let wireSpecs = tools.map { WireToolSpec(tool: $0, forceDeferred: true) }
        let json = try encodeJSONString(wireSpecs)
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = json.withCString { cstr in
            jimu_session_upsert_tools(session, cstr, &errorPtr)
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        bridge.upsertTools(tools)
    }

    /// Removes deferred tools by name while idle.
    @discardableResult
    public func removeTools(named names: [String]) throws -> Int {
        let session = try requireSession()
        let residentNames = Set(try fetchInfo().residentTools.map(\.name))
        let removableNames = names.filter { !residentNames.contains($0) }
        let json = try encodeJSONString(removableNames)
        var removed: Int = 0
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = json.withCString { cstr in
            jimu_session_remove_tools(session, cstr, &removed, &errorPtr)
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        bridge.removeTools(named: removableNames)
        return removed
    }

    // MARK: - Compaction, Branching & Introspection

    /// Runs manual compaction immediately while idle. Returns `nil` when there is nothing to compact.
    public func compact() async throws -> JimuCompactionResult? {
        let session = try requireSession()
        return try await withCheckedThrowingContinuation { continuation in
            let box = CompactionContinuationBox(continuation)
            let boxPtr = Unmanaged.passRetained(box).toOpaque()
            jimu_session_compact(session, boxPtr, jimuCompactDoneTrampoline)
        }
    }

    /// Branches the session to an earlier `entryID` without deleting sibling entries.
    public func fork(entryID: String) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = entryID.withCString { cstr in
            jimu_session_fork(session, cstr, &errorPtr)
        }
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Returns all persisted entries in append order.
    public func entries() throws -> [JimuSessionEntry] {
        let session = try requireSession()
        var jsonPtr: UnsafeMutablePointer<CChar>?
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = jimu_session_entries(session, &jsonPtr, &errorPtr)
        if status != JIMU_OK {
            _ = takeOwnedCString(jsonPtr)
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        guard let jsonStr = takeOwnedCString(jsonPtr),
              let data = jsonStr.data(using: .utf8)
        else {
            return []
        }
        let rawList = try JSONDecoder().decode([JSONValue].self, from: data)
        return try rawList.map(JimuSessionEntry.init)
    }

    /// Returns the session's stable identifier.
    public func sessionID() throws -> String {
        try fetchInfo().sessionID
    }

    /// Returns the active branch leaf entry identifier, if any.
    public func leafID() throws -> String? {
        try fetchInfo().leafID
    }

    /// Returns the currently configured model.
    public func currentModel() throws -> JimuModel {
        try fetchInfo().model
    }

    /// Returns the current thinking level.
    public func currentReasoningEffort() throws -> JimuThinkingLevel {
        try fetchInfo().thinkingLevel
    }

    /// Returns whether fast mode is enabled.
    public func isFast() throws -> Bool {
        try fetchInfo().fast
    }

    /// Reloads file-backed QuickJS extensions atomically while idle.
    public func reloadExtensions() throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = jimu_session_reload_extensions(session, &errorPtr)
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Re-scans configured `skillRoots` and updates the `"skills"` context block if changed.
    @discardableResult
    public func reloadSkills() throws -> Bool {
        let session = try requireSession()
        var changed = false
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = jimu_session_reload_skills(session, &changed, &errorPtr)
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        return changed
    }

    /// Refreshes instructions for the next prompt. Returns whether the catalog changed.
    @discardableResult
    public func reloadInstructions() throws -> Bool {
        let session = try requireSession()
        var changed = false
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = jimu_session_reload_instructions(session, &changed, &errorPtr)
        if status != JIMU_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        return changed
    }

    /// Cancels any active run, awaits full teardown, and frees all underlying Rust & bridge resources.
    public func close() async {
        guard !isClosed else { return }
        isClosed = true
        if let session = handleBox.session {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                let box = VoidContinuationBox(continuation)
                let boxPtr = Unmanaged.passRetained(box).toOpaque()
                jimu_session_close(session, boxPtr, jimuVoidDoneTrampoline)
            }
        }
        for environment in workspaceEnvironments {
            await environment.shutdown()
        }
        backgroundForwarder?.cancel()
        handleBox.destroyNow()
    }

    private func fetchInfo() throws -> WireSessionInfo {
        let session = try requireSession()
        var jsonPtr: UnsafeMutablePointer<CChar>?
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = jimu_session_info(session, &jsonPtr, &errorPtr)
        if status != JIMU_OK {
            _ = takeOwnedCString(jsonPtr)
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        guard let jsonStr = takeOwnedCString(jsonPtr),
              let data = jsonStr.data(using: .utf8)
        else {
            throw JimuError.internalError("Empty session info JSON")
        }
        return try JSONDecoder().decode(WireSessionInfo.self, from: data)
    }
}
