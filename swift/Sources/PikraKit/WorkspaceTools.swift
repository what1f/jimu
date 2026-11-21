import Foundation

/// Reusable workspace tools with optional approval for calls that change state.
public struct PikraWorkspaceTools: Sendable {
    public enum ToolKind: Sendable, Hashable {
        case readFile, writeFile, editFile, applyPatch, findFiles, grep
        case shell, execCommand, writeStdin, monitor, sleep, webFetch
    }

    public let workspace: URL
    public let approval: PikraToolApproval?
    public let pluginID: String
    public let environment: (@Sendable () async -> [String: String])?
    public let shellOutputLimit: Int
    @usableFromInline
    let native: Result<PikraWorkspaceEnvironment, any Error>

    /// The underlying workspace environment created for this tool set, if initialization succeeded.
    public var workspaceEnvironment: PikraWorkspaceEnvironment? {
        try? native.get()
    }

    public init(
        workspace: URL,
        approval: PikraToolApproval? = nil,
        pluginID: String = "pikra-workspace",
        environment: (@Sendable () async -> [String: String])? = nil,
        shellOutputLimit: Int = 1024 * 1024
    ) {
        self.workspace = workspace.standardizedFileURL
        self.approval = approval
        self.pluginID = pluginID
        self.environment = environment
        self.shellOutputLimit = max(1, shellOutputLimit)
        native = Result {
            try PikraWorkspaceEnvironment(
                workspace: workspace,
                environment: environment,
                shellOutputLimit: max(1, shellOutputLimit)
            )
        }
    }

    /// Builds the requested tools in order, wrapping each with `approval` when configured.
    @_alwaysEmitIntoClient
    public func tools(
        _ kinds: [ToolKind] = [.readFile, .shell, .editFile, .writeFile]
    ) -> [PikraTool] {
        kinds.compactMap { kind in
            guard let environment = try? native.get(),
                  let (tool, effect) = try? Self.make(kind, environment: environment) else { return nil }
            return approval?.wrap(tool: tool, pluginID: pluginID, effect: effect) ?? tool
        }
    }

    @_alwaysEmitIntoClient
    static func make(
        _ kind: ToolKind,
        environment: PikraWorkspaceEnvironment
    ) throws -> (PikraTool, PikraToolEffect)? {
        switch kind {
        case .readFile: (try PikraBuiltinTools.read(environment), .readOnly)
        case .writeFile: (try PikraBuiltinTools.write(environment), .write)
        case .editFile: (try PikraBuiltinTools.edit(environment), .write)
        case .applyPatch: (try PikraBuiltinTools.applyPatch(environment), .write)
        case .findFiles: (try PikraBuiltinTools.find(environment), .readOnly)
        case .grep: (try PikraBuiltinTools.grep(environment), .readOnly)
        case .sleep: (try PikraBuiltinTools.sleep(environment), .readOnly)
        case .webFetch: (try PikraBuiltinTools.webFetch(environment), .readOnly)
        #if os(macOS)
        case .shell: (shell(try PikraBuiltinTools.bash(environment)), .dangerous)
        case .execCommand: (try PikraBuiltinTools.execCommand(environment), .dangerous)
        case .writeStdin: (try PikraBuiltinTools.writeStdin(environment), .dangerous)
        case .monitor: (try PikraBuiltinTools.monitor(environment), .dangerous)
        #else
        case .shell, .execCommand, .writeStdin, .monitor: nil
        #endif
        }
    }

    #if os(macOS)
    @usableFromInline
    static func shell(_ tool: PikraTool) -> PikraTool {
        var wrapped = PikraTool(
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
            executionMode: tool.executionMode
        ) { context, args in
            // Hosts may redact shell output after execution. Keep raw output out of progress events.
            let context = PikraToolCallContext(toolCallID: context.toolCallID, toolName: context.toolName)
            let result = try await tool.execute(context, args)
            guard let payload = result.structuredContent else { return result }
            return PikraToolOutput(
                content: [.text(try encodeJSONString(payload))],
                structuredContent: payload,
                isError: result.isError
            )
        }
        wrapped.associatedEnvironment = tool.associatedEnvironment
        return wrapped
    }
    #endif
}
