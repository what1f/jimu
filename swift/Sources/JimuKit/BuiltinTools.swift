import CJimu
import Foundation

/// Customizes a built-in tool's model-visible definition and default arguments.
public struct JimuToolDefinitionOverride: Encodable, Sendable {
    public enum Description: Encodable, Sendable {
        case replace(String)
        case append(String)

        private enum CodingKeys: String, CodingKey { case replace, append }

        public func encode(to encoder: any Encoder) throws {
            var container = encoder.container(keyedBy: CodingKeys.self)
            switch self {
            case .replace(let text): try container.encode(text, forKey: .replace)
            case .append(let text): try container.encode(text, forKey: .append)
            }
        }
    }

    public var rename: String?
    public var description: Description?
    public var hiddenParameters: [String]
    public var parameterDescriptions: [String: String]
    public var defaultArguments: [String: JSONValue]

    public init(
        rename: String? = nil,
        description: Description? = nil,
        hiddenParameters: [String] = [],
        parameterDescriptions: [String: String] = [:],
        defaultArguments: [String: JSONValue] = [:]
    ) {
        self.rename = rename
        self.description = description
        self.hiddenParameters = hiddenParameters
        self.parameterDescriptions = parameterDescriptions
        self.defaultArguments = defaultArguments
    }

    private enum CodingKeys: String, CodingKey {
        case rename, description
        case hiddenParameters = "hidden_parameters"
        case parameterDescriptions = "parameter_descriptions"
        case defaultArguments = "default_arguments"
    }
}

/// Output or completion from a background command or monitor.
public struct JimuBackgroundEvent: Decodable, Sendable, Equatable {
    public enum Kind: String, Decodable, Sendable {
        case monitorLines = "monitor_lines"
        case monitorExited = "monitor_exited"
        case shellExited = "shell_exited"
    }

    public let type: Kind
    public let id: String?
    public let name: String?
    public let lines: String?
    public let tailLines: String?
    public let logPath: String?
    public let sessionID: UInt32?
    public let command: String?
    public let exitCode: Int32?
    public let reason: String?

    private enum CodingKeys: String, CodingKey {
        case type, id, name, lines, command, reason
        case tailLines = "tail_lines"
        case logPath = "log_path"
        case sessionID = "session_id"
        case exitCode = "exit_code"
    }
}

/// Shared working directory, HTTP client and lifetime for a set of built-in tools.
public final class JimuWorkspaceEnvironment: @unchecked Sendable {
    public let workspace: URL
    /// One consumer receives up to 256 buffered events. New events replace the oldest when full.
    public let backgroundEvents: AsyncStream<JimuBackgroundEvent>
    fileprivate let raw: OpaquePointer
    private let events: AsyncStream<JimuBackgroundEvent>.Continuation

    public static let defaultMaxOutputBytes: Int = 50 * 1024

    public convenience init(
        workspace: URL,
        maxOutputBytes: Int = JimuWorkspaceEnvironment.defaultMaxOutputBytes,
        environment: (@Sendable () async -> [String: String])? = nil,
        httpClient: any JimuHTTPClient = JimuURLSessionHTTPClient()
    ) throws {
        try self.init(
            workspace: workspace,
            maxOutputBytes: maxOutputBytes,
            environment: environment,
            httpClient: httpClient,
            shellOutputLimit: nil
        )
    }

    init(
        workspace: URL,
        maxOutputBytes: Int = JimuWorkspaceEnvironment.defaultMaxOutputBytes,
        environment: (@Sendable () async -> [String: String])?,
        httpClient: any JimuHTTPClient = JimuURLSessionHTTPClient(),
        shellOutputLimit: Int?
    ) throws {
        let workspace = workspace.standardizedFileURL
        guard workspace.isFileURL, maxOutputBytes > 0 else {
            throw JimuError.invalidArgument("Specify a workspace directory and a positive output limit")
        }
        var config: [String: JSONValue] = [
            "cwd": .string(workspace.path),
            "max_output_bytes": .integer(Int64(maxOutputBytes)),
        ]
        if let shellOutputLimit {
            config["default_exec_timeout_ms"] = .integer(120_000)
            config["shell_max_output_bytes"] = .integer(Int64(shellOutputLimit))
        }
        let json = try encodeJSONString(config)
        let (stream, events) = AsyncStream<JimuBackgroundEvent>.makeStream(bufferingPolicy: .bufferingNewest(256))
        let host = BuiltinHost(environment: environment, events: events)
        let hostPointer = Unmanaged.passRetained(host).toOpaque()
        let httpBridge = JimuSessionBridge(
            httpClient: httpClient, authorizeRequest: nil, beforeRequest: nil, initialTools: []
        )
        let httpPointer = Unmanaged.passRetained(httpBridge).toOpaque()
        var callbacks = JimuBuiltinCallbacks(
            ctx: hostPointer,
            environment: environment == nil ? nil : builtinEnvironment,
            background_event: builtinBackgroundEvent,
            destroy: builtinHostDestroy
        )
        var http = JimuHttpCallbacks(
            ctx: httpPointer,
            start: jimuHttpStartTrampoline,
            cancel: jimuHttpCancelTrampoline,
            destroy: jimuBridgeDestroyTrampoline
        )
        var error: UnsafeMutablePointer<CChar>?
        guard let raw = json.withCString({ jimu_builtin_env_create($0, &callbacks, &http, &error) }) else {
            Unmanaged<BuiltinHost>.fromOpaque(hostPointer).release()
            Unmanaged<JimuSessionBridge>.fromOpaque(httpPointer).release()
            throw JimuError.invalidArgument(takeOwnedCString(error) ?? "Could not open workspace tools")
        }
        self.raw = raw
        self.workspace = workspace
        self.backgroundEvents = stream
        self.events = events
    }

    /// Cancels active tools and waits for subprocesses to exit.
    public func shutdown() async {
        await withCheckedContinuation { continuation in
            let box = VoidContinuationBox(continuation)
            jimu_builtin_env_shutdown(raw, Unmanaged.passRetained(box).toOpaque(), jimuVoidDoneTrampoline)
        }
        events.finish()
    }

    func beginShutdown() {
        jimu_builtin_env_shutdown(raw, nil, nil)
        events.finish()
    }

    deinit {
        jimu_builtin_env_destroy(raw)
        events.finish()
    }
}

private final class BuiltinHost: Sendable {
    let environment: (@Sendable () async -> [String: String])?
    let events: AsyncStream<JimuBackgroundEvent>.Continuation

    init(environment: (@Sendable () async -> [String: String])?, events: AsyncStream<JimuBackgroundEvent>.Continuation) {
        self.environment = environment
        self.events = events
    }
}

private let builtinEnvironment: JimuBuiltinEnvProviderFn = { context, completion in
    guard let context, let completion else { return }
    let host = Unmanaged<BuiltinHost>.fromOpaque(context).takeUnretainedValue()
    let completionPointer = SendableOpaquePointer(raw: UnsafeMutableRawPointer(completion))
    Task {
        let values = await host.environment?() ?? [:]
        if let json = try? encodeJSONString(values) {
            json.withCString { jimu_builtin_env_complete(OpaquePointer(completionPointer.raw), $0) }
        } else {
            jimu_builtin_env_complete(OpaquePointer(completionPointer.raw), nil)
        }
    }
}

private let builtinBackgroundEvent: JimuEventFn = { context, json in
    guard let context, let json else { return }
    let host = Unmanaged<BuiltinHost>.fromOpaque(context).takeUnretainedValue()
    if let event = try? JSONDecoder().decode(JimuBackgroundEvent.self, from: Data(String(cString: json).utf8)) {
        host.events.yield(event)
    }
}

private let builtinHostDestroy: JimuVoidDoneFn = { context in
    if let context {
        Unmanaged<BuiltinHost>.fromOpaque(context).release()
    }
}

/// Create only the tools the host registers. Each factory links its Rust implementation independently.
public enum JimuBuiltinTools {
    // Keep each C symbol reference in clients that call its factory. Swift otherwise
    // retains public Swift functions when statically linking the package.
    @_alwaysEmitIntoClient
    public static func read(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_read, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func write(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_write, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func edit(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_edit, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func applyPatch(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_apply_patch, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func find(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_find, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func grep(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_grep, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func sleep(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_sleep, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func webFetch(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_web_fetch, environment, override)
    }

    #if os(macOS)
    @_alwaysEmitIntoClient
    public static func execCommandOneShot(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_exec_command_oneshot, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func execCommand(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_exec_command, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func writeStdin(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_write_stdin, environment, override)
    }

    @_alwaysEmitIntoClient
    public static func monitor(_ environment: JimuWorkspaceEnvironment, override: JimuToolDefinitionOverride? = nil) throws -> JimuTool {
        try make(jimu_builtin_tool_monitor, environment, override)
    }
    #endif

    @usableFromInline
    typealias Factory = (OpaquePointer?, UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) -> OpaquePointer?

    private struct Definition: Decodable {
        let name: String
        let description: String
        let parameters: JSONValue
    }

    @usableFromInline
    static func make(
        _ factory: Factory,
        _ environment: JimuWorkspaceEnvironment,
        _ override: JimuToolDefinitionOverride?
    ) throws -> JimuTool {
        var error: UnsafeMutablePointer<CChar>?
        guard let raw = factory(environment.raw, &error) else {
            throw JimuError.invalidArgument(takeOwnedCString(error) ?? "Could not create the tool")
        }
        let handle: BuiltinToolHandle
        if let override {
            defer { jimu_builtin_tool_destroy(raw) }
            let spec = try encodeJSONString(override)
            guard let wrapped = spec.withCString({ jimu_builtin_tool_override(raw, $0, &error) }) else {
                throw JimuError.invalidArgument(takeOwnedCString(error) ?? "Invalid tool definition")
            }
            handle = BuiltinToolHandle(wrapped, environment: environment)
        } else {
            handle = BuiltinToolHandle(raw, environment: environment)
        }
        guard let json = takeOwnedCString(jimu_builtin_tool_definition_json(handle.raw)) else {
            throw JimuError.internalError("Could not load the tool definition")
        }
        let definition = try JSONDecoder().decode(Definition.self, from: Data(json.utf8))
        var tool = JimuTool(
            name: definition.name,
            description: definition.description,
            parameters: definition.parameters,
            executionMode: jimu_builtin_tool_is_sequential(handle.raw) ? .sequential : .parallel
        ) { context, args in
            try await handle.execute(context, args)
        }
        tool.associatedEnvironment = environment
        return tool
    }
}

private final class BuiltinToolHandle: @unchecked Sendable {
    let raw: OpaquePointer
    private let environment: JimuWorkspaceEnvironment

    init(_ raw: OpaquePointer, environment: JimuWorkspaceEnvironment) {
        self.raw = raw
        self.environment = environment
    }

    deinit { jimu_builtin_tool_destroy(raw) }

    func execute(_ context: JimuToolCallContext, _ arguments: JSONValue) async throws -> JimuToolOutput {
        try Task.checkCancellation()
        let json = try encodeJSONString(arguments)
        let cancellation = BuiltinCancellation()
        let result: JimuToolOutput = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                let call = BuiltinCall(continuation: continuation, context: context)
                let callPointer = Unmanaged.passRetained(call).toOpaque()
                var error: UnsafeMutablePointer<CChar>?
                let run = context.toolCallID.withCString { id in
                    json.withCString { args in
                        jimu_builtin_tool_execute(raw, id, args, callPointer, builtinProgress, builtinDone, &error)
                    }
                }
                if let run {
                    cancellation.install(run)
                } else {
                    Unmanaged<BuiltinCall>.fromOpaque(callPointer).release()
                    continuation.resume(throwing: JimuError.invalidArgument(takeOwnedCString(error) ?? "Could not run the tool"))
                }
            }
        } onCancel: {
            cancellation.cancel()
        }
        try Task.checkCancellation()
        return result
    }
}

private final class BuiltinCancellation: @unchecked Sendable {
    private let lock = NSLock()
    private var run: OpaquePointer?
    private var cancelled = false

    func install(_ run: OpaquePointer) {
        lock.withLock {
            self.run = run
            if cancelled { jimu_builtin_tool_cancel(run) }
        }
    }

    func cancel() {
        lock.withLock {
            cancelled = true
            if let run { jimu_builtin_tool_cancel(run) }
        }
    }

    deinit {
        if let run { jimu_builtin_run_destroy(run) }
    }
}

private final class BuiltinCall: Sendable {
    let continuation: CheckedContinuation<JimuToolOutput, any Error>
    let context: JimuToolCallContext

    init(continuation: CheckedContinuation<JimuToolOutput, any Error>, context: JimuToolCallContext) {
        self.continuation = continuation
        self.context = context
    }
}

private let builtinProgress: JimuEventFn = { context, json in
    guard let context, let json else { return }
    let call = Unmanaged<BuiltinCall>.fromOpaque(context).takeUnretainedValue()
    if let output = try? JSONDecoder().decode(JimuToolOutput.self, from: Data(String(cString: json).utf8)) {
        call.context.reportProgress(output)
    }
}

private let builtinDone: JimuResultDoneFn = { context, status, json, error in
    guard let context else { return }
    let call = Unmanaged<BuiltinCall>.fromOpaque(context).takeRetainedValue()
    if status == JIMU_ERR_CANCELLED {
        call.continuation.resume(throwing: CancellationError())
    } else if status == JIMU_OK, let json {
        call.continuation.resume(with: Result {
            try JSONDecoder().decode(JimuToolOutput.self, from: Data(String(cString: json).utf8))
        })
    } else {
        call.continuation.resume(throwing: mapStatusError(status, message: error.map { String(cString: $0) }))
    }
}
