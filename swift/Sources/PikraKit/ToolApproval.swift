import Foundation

/// Effect classification assigned by the host to a tool implementation.
public enum PikraToolEffect: Sendable, Equatable {
    case readOnly
    case write
    case dangerous
}

/// Authorizes host tool closures with readOnly/write/dangerous grants & revocation versioning.
public actor PikraToolApproval {
    public struct Request: Sendable {
        public let pluginID: String
        public let toolName: String
        public let input: JSONValue
        public let context: PikraToolCallContext
        public let effect: PikraToolEffect

        public init(
            pluginID: String,
            toolName: String,
            input: JSONValue,
            context: PikraToolCallContext,
            effect: PikraToolEffect
        ) {
            self.pluginID = pluginID
            self.toolName = toolName
            self.input = input
            self.context = context
            self.effect = effect
        }
    }

    public enum Decision: Sendable, Equatable {
        case allowOnce
        case allowPlugin
        case deny
    }

    public enum Failure: Error, Sendable, Equatable, LocalizedError {
        case denied

        public var errorDescription: String? {
            switch self {
            case .denied: return "Tool execution denied"
            }
        }
    }

    private let onApproval: @Sendable (Request) async throws -> Decision
    private var grants: Set<String> = []
    private var revocations: [String: UInt64] = [:]

    public init(onApproval: @escaping @Sendable (Request) async throws -> Decision) {
        self.onApproval = onApproval
    }

    /// Snapshot of plugin IDs currently granted persistent write approval.
    public var allowedPlugins: Set<String> {
        grants
    }

    /// Pre-grants write approval for a plugin (for example restored from host preferences).
    public func allow(pluginID: String) {
        grants.insert(pluginID)
    }

    /// Revokes write approval and increments the plugin's revocation epoch so in-flight
    /// approval prompts cannot resurrect a stale grant.
    public func revoke(pluginID: String) {
        grants.remove(pluginID)
        revocations[pluginID, default: 0] += 1
    }

    /// Wraps a `PikraTool`, preserving its descriptor and authorizing each invocation.
    public nonisolated func wrap(
        tool: PikraTool,
        pluginID: String,
        effect: PikraToolEffect
    ) -> PikraTool {
        var wrapped = PikraTool(
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
            deferred: tool.deferred,
            executionMode: tool.executionMode
        ) { context, input in
            try await self.authorize(
                Request(
                    pluginID: pluginID,
                    toolName: tool.name,
                    input: input,
                    context: context,
                    effect: effect
                )
            )
            try Task.checkCancellation()
            return try await tool.execute(context, input)
        }
        wrapped.associatedEnvironment = tool.associatedEnvironment
        return wrapped
    }

    private func authorize(_ request: Request) async throws {
        try Task.checkCancellation()
        if request.effect == .readOnly {
            return
        }
        if request.effect == .write, grants.contains(request.pluginID) {
            return
        }

        let version = revocations[request.pluginID, default: 0]
        let decision = try await onApproval(request)
        try Task.checkCancellation()
        switch decision {
        case .deny:
            throw Failure.denied
        case .allowOnce:
            break
        case .allowPlugin:
            if request.effect == .write, revocations[request.pluginID, default: 0] == version {
                grants.insert(request.pluginID)
            }
        }
    }
}
