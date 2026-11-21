import Foundation
import Network
import Testing
import PikraKit

private func inWorkspace(_ operation: (URL) async throws -> Void) async throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    try await operation(directory)
}

private func context(for tool: PikraTool) -> PikraToolCallContext {
    PikraToolCallContext(toolCallID: UUID().uuidString, toolName: tool.name)
}

@Test("Discover instructions with an unlimited or UTF-8-safe byte budget")
func discoverInstructions() async throws {
    try await inWorkspace { workspace in
        let file = workspace.appendingPathComponent("AGENTS.override.md")
        let fallback = workspace.appendingPathComponent("AGENTS.md")
        let duplicate = workspace.appendingPathComponent("duplicate.md")
        let content = "a" + String(repeating: "瑞", count: 12_000)
        try content.write(to: file, atomically: true, encoding: .utf8)
        try "fallback".write(to: fallback, atomically: true, encoding: .utf8)
        try ("\u{FEFF}" + content + "\r\n").write(to: duplicate, atomically: true, encoding: .utf8)
        var config = PikraInstructionDiscovery.Configuration(
            cwd: workspace, walkBoundary: .currentDirOnly, includeUserInstructions: false,
            extraFiles: [duplicate], maxBytes: nil
        )
        let unlimited = try PikraInstructionDiscovery.discover(config: config)
        #expect(unlimited.entries.map(\.content) == [content])
        #expect(unlimited.entries.first?.path.resolvingSymlinksInPath() == file.resolvingSymlinksInPath())
        #expect(unlimited.totalBytes == content.utf8.count)
        #expect(!unlimited.truncated)
        config.maxBytes = 3
        let limited = try PikraInstructionDiscovery.discover(config: config)
        #expect(limited.entries.first?.content == "a")
        #expect(limited.entries.first?.truncated == true)
        #expect(limited.truncated)
    }
}

@Test("Reload workspace instructions and carry them through compaction")
func agentInstructionContext() async throws {
    try await inWorkspace { workspace in
        let file = workspace.appendingPathComponent("AGENTS.md")
        try "first workspace rule".write(to: file, atomically: true, encoding: .utf8)
        let reply: MockSSEClient.Handler = { _, _ in
            MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Acknowledged."))
        }
        let client = MockSSEClient(handlers: Array(repeating: reply, count: 5))
        var config = PikraConfiguration(
            model: testModel(), cwd: workspace.path,
            compaction: .init(keepRecentTokens: 1), httpClient: client
        )
        config.instructionDiscovery = .init(includeUserInstructions: false)
        let agent = try await PikraAgent.open(configuration: config)
        func prompt(_ text: String) async throws -> [String] {
            var instructions: [String] = []
            for try await event in try await agent.prompt(text) {
                if case .contextAppended(let name, let content) = event, name == "instructions" {
                    instructions.append(content)
                }
            }
            return instructions
        }
        #expect(try await prompt("First task").first?.contains("first workspace rule") == true)
        #expect(try await agent.reloadInstructions() == false)
        try "revised workspace rule".write(to: file, atomically: true, encoding: .utf8)
        #expect(try await agent.reloadInstructions())
        #expect(try await prompt("Second task").first?.contains("revised workspace rule") == true)
        let result = try await agent.compact()
        try #require(result != nil)
        #expect(try await prompt("After compaction").first?.contains("revised workspace rule") == true)
        try FileManager.default.removeItem(at: file)
        #expect(try await agent.reloadInstructions())
        #expect(try await prompt("After removal").first?.contains("no longer apply") == true)
        let requests = await client.recordedRequests()
        try #require(requests.count == 5)
        let compactedBody = String(decoding: requests[3].body, as: UTF8.self)
        #expect(compactedBody.contains("revised workspace rule"))
        await agent.close()
    }
}

@Test("Apply a patch and search the new file with an overridden Rust grep tool")
func patchAndGrepWorkspace() async throws {
    try await inWorkspace { workspace in
        let patch = try #require(PikraWorkspaceTools(workspace: workspace).tools([.applyPatch]).first)
        let patched = try await patch.execute(context(for: patch), ["patch": """
            *** Begin Patch
            *** Add File: notes.txt
            +first line
            +fixture match
            *** End Patch
            """])
        #expect(!patched.isError)

        let environment = try PikraWorkspaceEnvironment(workspace: workspace)
        let grep = try PikraBuiltinTools.grep(environment, override: .init(
            rename: "search_notes",
            description: .append("Search the notes file."),
            hiddenParameters: ["path"],
            parameterDescriptions: ["pattern": "Search expression."],
            defaultArguments: ["path": "notes.txt"]
        ))
        #expect(grep.name == "search_notes")
        #expect(grep.description.hasSuffix("Search the notes file."))
        #expect(grep.parameters["properties"]?["path"] == nil)
        #expect(grep.parameters["properties"]?["pattern"]?["description"] == "Search expression.")
        let matches = try await grep.execute(context(for: grep), ["pattern": "fixture", "path": "absent.txt"])
        #expect(!matches.isError)
        #expect(matches.textOutput.contains("notes.txt:2:fixture match"))
        await environment.shutdown()
    }
}

#if os(macOS)
@Test("Monitor wakes a Rust sleep and stdin reaches the managed background command", .timeLimit(.minutes(1)))
func backgroundWorkspaceTools() async throws {
    try await inWorkspace { workspace in
        let environment = try PikraWorkspaceEnvironment(workspace: workspace)
        let sleep = try PikraBuiltinTools.sleep(environment)
        let monitor = try PikraBuiltinTools.monitor(environment)
        let exec = try PikraBuiltinTools.execCommand(environment)
        let stdin = try PikraBuiltinTools.writeStdin(environment)
        var config = PikraConfiguration(model: .init(
            id: "fixture", provider: "fixture", api: .openaiResponses, baseURL: "https://example.invalid",
            contextWindow: 32_000, maxOutputTokens: 2_000, supportsImages: false, supportsReasoning: false
        ))
        config.workspaceEnvironment = environment
        let agent = try await PikraAgent.open(configuration: config)
        let started = AsyncStream<Void>.makeStream()
        let waiting = Task {
            let context = PikraToolCallContext(toolCallID: "wait", toolName: sleep.name) { _ in
                started.continuation.finish()
            }
            return try await sleep.execute(context, ["duration_ms": 60_000, "reason": "Waiting for monitor"])
        }
        defer { waiting.cancel() }
        for await _ in started.stream {}

        let launched = try await monitor.execute(context(for: monitor), [
            "action": "start", "name": "fixture", "command": "printf 'monitor result\\n'", "notify": "exit",
        ])
        try #require(!launched.isError)
        let result = try await waiting.value
        #expect(result.details["woken_early"] == true)

        var events = agent.backgroundEvents.makeAsyncIterator()
        let monitored = try #require(await events.next())
        #expect(monitored.type == .monitorExited)
        #expect(monitored.tailLines?.contains("monitor result") == true)

        let process = try await exec.execute(context(for: exec), [
            "command": #"IFS= read -r line; printf 'received %s\n' "$line""#,
            "background": true,
        ])
        let sessionID = try #require(process.details["session_id"])
        let sent = try await stdin.execute(context(for: stdin), [
            "session_id": sessionID, "chars": "hello\n", "yield_time_ms": 60_000,
        ])
        #expect(!sent.isError)
        #expect(sent.textOutput.contains("received hello"))
        let exited = try #require(await events.next())
        #expect(exited.type == .shellExited)
        #expect(exited.exitCode == 0)

        // Shutdown must reap a child that still has its stdin open.
        let pending = try await exec.execute(context(for: exec), [
            "command": "read line", "background": true,
        ])
        try #require(!pending.isError)
        await agent.close()
        #expect(await events.next() == nil)
    }
}
#endif

@Test("URLSession protects credentials and Rust web_fetch checks each redirect destination")
func workspaceWebFetchRedirects() async throws {
    let listener = try NWListener(using: .tcp)
    defer {
        listener.newConnectionHandler = nil
        listener.cancel()
    }
    let (ready, state) = AsyncThrowingStream<Void, any Error>.makeStream()
    let queue = DispatchQueue(label: "Pikra HTTP test fixture")
    listener.stateUpdateHandler = { result in
        switch result {
        case .ready: state.yield(()); state.finish()
        case .failed(let error): state.finish(throwing: error)
        default: break
        }
    }
    listener.newConnectionHandler = { connection in
        connection.start(queue: queue)
        reply(to: connection, port: listener.port?.rawValue ?? 0)
    }
    listener.start(queue: queue)
    for try await _ in ready {}
    let port = try #require(listener.port?.rawValue)
    let base = try #require(URL(string: "http://127.0.0.1:\(port)"))
    let transport = PikraURLSessionHTTPClient()

    func status(_ path: String, headers: [(String, String)] = []) async throws -> Int {
        let response = try await transport.send(.init(method: "GET", url: base.appendingPathComponent(path), headers: headers))
        response.cancel()
        return response.statusCode
    }
    #expect(try await status("redirect") == 302)
    #expect(try await status("same-origin", headers: [("Authorization", "Bearer fixture")]) == 200)
    #expect(try await status("cross-origin", headers: [("x-api-key", "fixture")]) == 302)

    try await inWorkspace { workspace in
        // Only the first, public fixture destination is mapped to the local server.
        let client = LocalFixtureHTTPClient(base: base, transport: transport)
        let environment = try PikraWorkspaceEnvironment(workspace: workspace, httpClient: client)
        let fetch = try PikraBuiltinTools.webFetch(environment)
        let page = try await fetch.execute(context(for: fetch), ["url": "http://8.8.8.8/content", "raw": true])
        #expect(!page.isError)
        #expect(page.textOutput.contains("public fixture"))
        let blocked = try await fetch.execute(context(for: fetch), ["url": "http://8.8.8.8/blocked", "raw": true])
        #expect(blocked.isError)
        #expect(blocked.textOutput.localizedCaseInsensitiveContains("not public"))
        #expect(await client.requestedPaths() == ["/content", "/blocked"])
        await environment.shutdown()
    }
}

private actor LocalFixtureHTTPClient: PikraHTTPClient {
    let base: URL
    let transport: PikraURLSessionHTTPClient
    private var paths: [String] = []

    init(base: URL, transport: PikraURLSessionHTTPClient) {
        self.base = base
        self.transport = transport
    }

    func send(_ request: PikraHTTPRequest) async throws -> PikraHTTPResponse {
        paths.append(request.url.path)
        guard request.url.host == "8.8.8.8" else {
            throw URLError(.unsupportedURL)
        }
        return try await transport.send(.init(
            method: request.method, url: base.appendingPathComponent(request.url.lastPathComponent),
            headers: request.headers, timeoutInterval: request.timeoutInterval
        ))
    }

    func requestedPaths() -> [String] { paths }
}

private func reply(to connection: NWConnection, port: UInt16, received: Data = Data()) {
    connection.receive(minimumIncompleteLength: 1, maximumLength: 8 * 1024) { data, _, complete, error in
        let received = received + (data ?? Data())
        let request = String(decoding: received, as: UTF8.self)
        guard request.contains("\r\n\r\n") else {
            if complete || error != nil || received.count > 16 * 1024 {
                connection.cancel()
            } else {
                reply(to: connection, port: port, received: received)
            }
            return
        }
        let response: String
        if request.hasPrefix("GET /redirect ") || request.hasPrefix("GET /blocked ") {
            response = "302 Found\r\nLocation: http://127.0.0.1:\(port)/content\r\nContent-Length: 0\r\n\r\n"
        } else if request.hasPrefix("GET /same-origin ") {
            response = "302 Found\r\nLocation: /protected\r\nContent-Length: 0\r\n\r\n"
        } else if request.hasPrefix("GET /cross-origin ") {
            response = "302 Found\r\nLocation: http://localhost:\(port)/content\r\nContent-Length: 0\r\n\r\n"
        } else if request.hasPrefix("GET /protected ") && !request.lowercased().contains("authorization: bearer fixture\r\n") {
            response = "401 Unauthorized\r\nContent-Length: 0\r\n\r\n"
        } else {
            let body = "public fixture"
            response = "200 OK\r\nContent-Type: text/plain\r\nContent-Length: \(body.utf8.count)\r\n\r\n\(body)"
        }
        connection.send(content: Data("HTTP/1.1 \(response)".utf8), contentContext: .finalMessage, isComplete: true,
                        completion: .contentProcessed { _ in connection.cancel() })
    }
}
