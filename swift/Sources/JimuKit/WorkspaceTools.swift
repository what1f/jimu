import Foundation

/// Reusable workspace tools with optional approval for calls that change state.
public struct JimuWorkspaceTools: Sendable {
    public enum ToolKind: Sendable, Hashable {
        case readFile, writeFile, editFile, applyPatch, findFiles, grep
        case shell, execCommand, writeStdin, monitor, sleep, webFetch
    }

    public let workspace: URL
    public let approval: JimuToolApproval?
    public let pluginID: String
    public let environment: (@Sendable () async -> [String: String])?
    public let shellOutputLimit: Int
    @usableFromInline
    let native: Result<JimuWorkspaceEnvironment, any Error>

    /// The underlying workspace environment created for this tool set, if initialization succeeded.
    public var workspaceEnvironment: JimuWorkspaceEnvironment? {
        try? native.get()
    }

    public init(
        workspace: URL,
        approval: JimuToolApproval? = nil,
        pluginID: String = "jimu-workspace",
        environment: (@Sendable () async -> [String: String])? = nil,
        shellOutputLimit: Int = JimuWorkspaceEnvironment.defaultMaxOutputBytes
    ) {
        self.workspace = workspace.standardizedFileURL
        self.approval = approval
        self.pluginID = pluginID
        self.environment = environment
        self.shellOutputLimit = max(1, shellOutputLimit)
        native = Result {
            try JimuWorkspaceEnvironment(
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
    ) -> [JimuTool] {
        kinds.compactMap { kind in
            guard let environment = try? native.get(),
                  let (tool, effect) = try? Self.make(kind, environment: environment) else { return nil }
            return approval?.wrap(tool: tool, pluginID: pluginID, effect: effect) ?? tool
        }
    }

    @_alwaysEmitIntoClient
    static func make(
        _ kind: ToolKind,
        environment: JimuWorkspaceEnvironment
    ) throws -> (JimuTool, JimuToolEffect)? {
        switch kind {
        case .readFile: (try JimuBuiltinTools.read(environment), .readOnly)
        case .writeFile: (try JimuBuiltinTools.write(environment), .write)
        case .editFile: (try JimuBuiltinTools.edit(environment), .write)
        case .applyPatch: (try JimuBuiltinTools.applyPatch(environment), .write)
        case .findFiles: (try JimuBuiltinTools.find(environment), .readOnly)
        case .grep: (try JimuBuiltinTools.grep(environment), .readOnly)
        case .sleep: (try JimuBuiltinTools.sleep(environment), .readOnly)
        case .webFetch: (try JimuBuiltinTools.webFetch(environment), .readOnly)
        #if os(macOS)
        case .shell:
            (
                shell(
                    try JimuBuiltinTools.execCommandOneShot(
                        environment,
                        override: JimuToolDefinitionOverride(rename: "bash")
                    )
                ),
                .dangerous
            )
        case .execCommand: (try JimuBuiltinTools.execCommand(environment), .dangerous)
        case .writeStdin: (try JimuBuiltinTools.writeStdin(environment), .dangerous)
        case .monitor: (try JimuBuiltinTools.monitor(environment), .dangerous)
        #else
        case .shell, .execCommand, .writeStdin, .monitor: nil
        #endif
        }
    }

    #if os(macOS)
    @usableFromInline
    static func shell(_ tool: JimuTool) -> JimuTool {
        var wrapped = JimuTool(
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
            executionMode: tool.executionMode
        ) { context, args in
            // Hosts may redact shell output after execution. Keep raw output out of progress events.
            let context = JimuToolCallContext(toolCallID: context.toolCallID, toolName: context.toolName)
            let result = try await tool.execute(context, args)
            guard let payload = result.structuredContent else { return result }
            return JimuToolOutput(
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
