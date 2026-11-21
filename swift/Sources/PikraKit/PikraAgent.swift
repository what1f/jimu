import CPikra
import Foundation

/// Swift 6 strict-concurrency actor driving a Pikra headless agent session over C-ABI (`CPikra`).
public actor PikraAgent {
    private final class HandleBox: @unchecked Sendable {
        var session: OpaquePointer?
        let bridge: PikraSessionBridge

        init(session: OpaquePointer, bridge: PikraSessionBridge) {
            self.session = session
            self.bridge = bridge
        }

        func destroyNow() {
            bridge.cancelAll()
            if let s = session {
                session = nil
                // Cancels any active Rust run and drops Swift's Arc<FfiSession>.
                // The HTTP callbacks retain the bridge until the last Rust request releases it.
                pikra_session_destroy(s)
            }
        }

        deinit {
            destroyNow()
        }
    }

    private let handleBox: HandleBox
    private let bridge: PikraSessionBridge
    private var isClosed = false
    private let workspaceEnvironment: PikraWorkspaceEnvironment?
    /// Background events from the workspace environment selected in the configuration.
    public nonisolated let backgroundEvents: AsyncStream<PikraBackgroundEvent>

    private init(
        session: OpaquePointer,
        bridge: PikraSessionBridge,
        workspaceEnvironment: PikraWorkspaceEnvironment?
    ) {
        self.handleBox = HandleBox(session: session, bridge: bridge)
        self.bridge = bridge
        self.workspaceEnvironment = workspaceEnvironment
        backgroundEvents = workspaceEnvironment?.backgroundEvents ?? AsyncStream { $0.finish() }
    }

    deinit {
        workspaceEnvironment?.beginShutdown()
    }

    /// Opens a new or existing `PikraAgent` session (in-memory or JSONL-persisted).
    public static func open(configuration: PikraConfiguration) async throws -> PikraAgent {
        try Task.checkCancellation()

        let httpClient = configuration.httpClient ?? PikraURLSessionHTTPClient()
        let bridge = PikraSessionBridge(
            httpClient: httpClient,
            authorizeRequest: configuration.authorizeRequest,
            beforeRequest: configuration.beforeRequest,
            initialTools: configuration.tools
        )
        let bridgePtr = Unmanaged.passRetained(bridge).toOpaque()

        let httpCallbacks = PikraHttpCallbacks(
            ctx: bridgePtr,
            start: pikraHttpStartTrampoline,
            cancel: pikraHttpCancelTrampoline,
            destroy: pikraBridgeDestroyTrampoline
        )
        let toolCallbacks = PikraToolCallbacks(
            ctx: bridgePtr,
            execute: pikraToolExecuteTrampoline,
            cancel: pikraToolCancelTrampoline
        )

        let configJSON: String
        do {
            configJSON = try encodeJSONString(WireSessionConfig(configuration))
        } catch {
            Unmanaged<PikraSessionBridge>.fromOpaque(bridgePtr).release()
            throw error
        }

        var errorPtr: UnsafeMutablePointer<CChar>?
        let sessionPtr = configJSON.withCString { cstr in
            pikra_session_create(cstr, httpCallbacks, toolCallbacks, &errorPtr)
        }
        guard let sessionPtr else {
            Unmanaged<PikraSessionBridge>.fromOpaque(bridgePtr).release()
            let message = takeOwnedCString(errorPtr) ?? "Failed to create PikraSession"
            throw PikraError.invalidArgument(message)
        }

        return PikraAgent(session: sessionPtr, bridge: bridge, workspaceEnvironment: configuration.workspaceEnvironment)
    }

    private func requireSession() throws -> OpaquePointer {
        guard !isClosed, let session = handleBox.session else {
            throw PikraError.closed
        }
        return session
    }

    // MARK: - Prompting & Streaming

    /// Sends a text prompt and returns a real-time `AsyncThrowingStream` of `PikraEvent`s.
    public func prompt(
        _ text: String,
        id: String = UUID().uuidString
    ) async throws -> AsyncThrowingStream<PikraEvent, any Error> {
        try await prompt([.text(text)], id: id)
    }

    /// Sends a multimodal prompt (text and/or images) and returns a real-time stream of `PikraEvent`s.
    public func prompt(
        _ content: [PikraPromptContent],
        id: String = UUID().uuidString
    ) async throws -> AsyncThrowingStream<PikraEvent, any Error> {
        let session = try requireSession()
        guard !content.isEmpty else {
            throw PikraError.invalidArgument("Prompt content must not be empty")
        }
        let wirePrompt = WirePromptRequest(id: id, content: content.map { $0.toWirePart() })
        let promptJSON = try encodeJSONString(wirePrompt)

        var capturedContinuation: AsyncThrowingStream<PikraEvent, any Error>.Continuation?
        let stream = AsyncThrowingStream<PikraEvent, any Error> { continuation in
            capturedContinuation = continuation
        }
        guard let continuation = capturedContinuation else {
            throw PikraError.internalError("Failed to initialize event stream")
        }

        let box = PromptStreamBox(continuation: continuation)
        let boxPtr = Unmanaged.passRetained(box).toOpaque()

        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = promptJSON.withCString { cstr in
            pikra_session_prompt_start(
                session,
                cstr,
                boxPtr,
                pikraPromptEventTrampoline,
                pikraPromptDoneTrampoline,
                &errorPtr
            )
        }
        if status != PIKRA_OK {
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
    public func steer(text: String, id: String = UUID().uuidString) -> Bool {
        guard !isClosed, let session = handleBox.session else { return false }
        return id.withCString { idC in
            text.withCString { textC in
                pikra_session_steer(session, idC, textC)
            }
        }
    }

    /// Queues a follow-up user message to execute after the current turn settles.
    public func followUp(text: String, id: String = UUID().uuidString) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = id.withCString { idC in
            text.withCString { textC in
                pikra_session_follow_up(session, idC, textC, &errorPtr)
            }
        }
        if status != PIKRA_OK {
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
            pikra_session_interrupt(session, boxPtr, pikraVoidDoneTrampoline)
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
                pikra_session_set_context(session, nameC, textC, &changed, &errorPtr)
            }
        }
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        return changed
    }

    /// Queues a one-off `<system-reminder>` context block for the next turn.
    public func appendContext(_ text: String) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = text.withCString { textC in
            pikra_session_append_context(session, textC, &errorPtr)
        }
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Switches the active model while idle and appends a `model_change` entry.
    public func setModel(_ model: PikraModel) throws {
        let session = try requireSession()
        let json = try encodeJSONString(model)
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = json.withCString { cstr in
            pikra_session_set_model(session, cstr, &errorPtr)
        }
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Updates reasoning / thinking effort while idle.
    public func setReasoningEffort(_ level: PikraThinkingLevel) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = level.rawValue.withCString { cstr in
            pikra_session_set_thinking_level(session, cstr, &errorPtr)
        }
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Toggles fast mode while idle.
    public func setFast(_ fast: Bool) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = pikra_session_set_fast(session, fast, &errorPtr)
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Updates or clears the active API key while idle (for token rotation without reopening).
    public func setAPIKey(_ apiKey: String?) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status: PikraStatus
        if let apiKey {
            status = apiKey.withCString { cstr in
                pikra_session_set_api_key(session, cstr, &errorPtr)
            }
        } else {
            status = pikra_session_set_api_key(session, nil, &errorPtr)
        }
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    // MARK: - Dynamic Deferred Tools

    /// Adds or replaces deferred tools while idle without mutating the resident `tools` array.
    public func upsertTools(_ tools: [PikraTool]) throws {
        let session = try requireSession()
        let wireSpecs = tools.map { WireToolSpec(tool: $0, forceDeferred: true) }
        let json = try encodeJSONString(wireSpecs)
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = json.withCString { cstr in
            pikra_session_upsert_tools(session, cstr, &errorPtr)
        }
        if status != PIKRA_OK {
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
            pikra_session_remove_tools(session, cstr, &removed, &errorPtr)
        }
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        bridge.removeTools(named: removableNames)
        return removed
    }

    // MARK: - Compaction, Branching & Introspection

    /// Runs manual compaction immediately while idle. Returns `nil` when there is nothing to compact.
    public func compact() async throws -> PikraCompactionResult? {
        let session = try requireSession()
        return try await withCheckedThrowingContinuation { continuation in
            let box = CompactionContinuationBox(continuation)
            let boxPtr = Unmanaged.passRetained(box).toOpaque()
            pikra_session_compact(session, boxPtr, pikraCompactDoneTrampoline)
        }
    }

    /// Branches the session to an earlier `entryID` without deleting sibling entries.
    public func fork(entryID: String) throws {
        let session = try requireSession()
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = entryID.withCString { cstr in
            pikra_session_fork(session, cstr, &errorPtr)
        }
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Returns all persisted entries in append order.
    public func entries() throws -> [PikraSessionEntry] {
        let session = try requireSession()
        var jsonPtr: UnsafeMutablePointer<CChar>?
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = pikra_session_entries(session, &jsonPtr, &errorPtr)
        if status != PIKRA_OK {
            _ = takeOwnedCString(jsonPtr)
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        guard let jsonStr = takeOwnedCString(jsonPtr),
              let data = jsonStr.data(using: .utf8)
        else {
            return []
        }
        let rawList = try JSONDecoder().decode([JSONValue].self, from: data)
        return try rawList.map(PikraSessionEntry.init)
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
    public func currentModel() throws -> PikraModel {
        try fetchInfo().model
    }

    /// Returns the current thinking level.
    public func currentReasoningEffort() throws -> PikraThinkingLevel {
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
        let status = pikra_session_reload_extensions(session, &errorPtr)
        if status != PIKRA_OK {
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
    }

    /// Re-scans configured `skillRoots` and updates the `"skills"` context block if changed.
    @discardableResult
    public func reloadSkills() throws -> Bool {
        let session = try requireSession()
        var changed = false
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = pikra_session_reload_skills(session, &changed, &errorPtr)
        if status != PIKRA_OK {
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
        let status = pikra_session_reload_instructions(session, &changed, &errorPtr)
        if status != PIKRA_OK {
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
                pikra_session_close(session, boxPtr, pikraVoidDoneTrampoline)
            }
        }
        await workspaceEnvironment?.shutdown()
        handleBox.destroyNow()
    }

    private func fetchInfo() throws -> WireSessionInfo {
        let session = try requireSession()
        var jsonPtr: UnsafeMutablePointer<CChar>?
        var errorPtr: UnsafeMutablePointer<CChar>?
        let status = pikra_session_info(session, &jsonPtr, &errorPtr)
        if status != PIKRA_OK {
            _ = takeOwnedCString(jsonPtr)
            throw mapStatusError(status, message: takeOwnedCString(errorPtr))
        }
        guard let jsonStr = takeOwnedCString(jsonPtr),
              let data = jsonStr.data(using: .utf8)
        else {
            throw PikraError.internalError("Empty session info JSON")
        }
        return try JSONDecoder().decode(WireSessionInfo.self, from: data)
    }
}
