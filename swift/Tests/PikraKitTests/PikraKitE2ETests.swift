import Foundation
import Testing
@testable import PikraKit

// MARK: - Deterministic Synchronization Primitives (Zero Task.sleep)

private actor Signal {
    private var signalled = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func fire() {
        signalled = true
        let pending = waiters
        waiters.removeAll()
        for waiter in pending {
            waiter.resume()
        }
    }

    func wait() async {
        if signalled { return }
        await withCheckedContinuation { continuation in
            if signalled {
                continuation.resume()
            } else {
                waiters.append(continuation)
            }
        }
    }
}

private final class CancellableGate: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, any Error>?
    private var cancelled = false

    func waitUntilCancelled() async throws {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, any Error>) in
                lock.withLock {
                    if cancelled {
                        cont.resume(throwing: CancellationError())
                    } else {
                        continuation = cont
                    }
                }
            }
        } onCancel: {
            let cont = lock.withLock { () -> CheckedContinuation<Void, any Error>? in
                cancelled = true
                let c = continuation
                continuation = nil
                return c
            }
            cont?.resume(throwing: CancellationError())
        }
    }
}

// MARK: - Deterministic Mock OpenAI Completions SSE Client

private actor MockSSEClient: PikraHTTPClient {
    typealias Handler = @Sendable (PikraHTTPRequest, Int) async throws -> PikraHTTPResponse

    private var requests: [PikraHTTPRequest] = []
    private var handlers: [Handler]

    init(handlers: [Handler]) {
        self.handlers = handlers
    }

    func recordedRequests() -> [PikraHTTPRequest] {
        requests
    }

    func send(_ request: PikraHTTPRequest) async throws -> PikraHTTPResponse {
        let index = requests.count
        requests.append(request)
        guard !handlers.isEmpty else {
            throw PikraError.transport("Unexpected HTTP request #\(index)")
        }
        let handler = handlers.removeFirst()
        return try await handler(request, index)
    }

    static func sseResponse(chunks: [String], statusCode: Int = 200) -> PikraHTTPResponse {
        let stream = AsyncThrowingStream<Data, any Error> { continuation in
            for chunk in chunks {
                continuation.yield(Data(chunk.utf8))
            }
            continuation.finish()
        }
        return PikraHTTPResponse(
            statusCode: statusCode,
            headers: [("content-type", "text/event-stream")],
            body: stream
        )
    }

    static func textChunks(
        text: String,
        thinking: String? = nil,
        promptTokens: Int = 40,
        completionTokens: Int = 12
    ) -> [String] {
        var chunks: [String] = []
        if let thinking {
            let escaped = jsonEscape(thinking)
            chunks.append(
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\(escaped)},\"finish_reason\":null}]}\n\n"
            )
        }
        let mid = text.index(text.startIndex, offsetBy: max(1, text.count / 2), limitedBy: text.endIndex) ?? text.endIndex
        let part1 = String(text[..<mid])
        let part2 = String(text[mid...])
        if !part1.isEmpty {
            chunks.append(
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\(jsonEscape(part1))},\"finish_reason\":null}]}\n\n"
            )
        }
        if !part2.isEmpty {
            chunks.append(
                "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\(jsonEscape(part2))},\"finish_reason\":null}]}\n\n"
            )
        }
        let total = promptTokens + completionTokens
        chunks.append(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":\(promptTokens),\"completion_tokens\":\(completionTokens),\"total_tokens\":\(total)}}\n\n"
        )
        chunks.append("data: [DONE]\n\n")
        return chunks
    }

    static func toolCallChunks(
        id: String,
        name: String,
        argumentsJSON: String,
        promptTokens: Int = 50,
        completionTokens: Int = 15
    ) -> [String] {
        let total = promptTokens + completionTokens
        return [
            "data: {\"id\":\"c_tc\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\(jsonEscape(id)),\"type\":\"function\",\"function\":{\"name\":\(jsonEscape(name)),\"arguments\":\(jsonEscape(argumentsJSON))}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c_tc\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":\(promptTokens),\"completion_tokens\":\(completionTokens),\"total_tokens\":\(total)}}\n\n",
            "data: [DONE]\n\n",
        ]
    }

    private static func jsonEscape(_ value: String) -> String {
        let data = try! JSONEncoder().encode(value)
        return String(data: data, encoding: .utf8)!
    }
}

private actor DataCollector {
    private var items: [Data] = []
    func append(_ data: Data) { items.append(data) }
    func all() -> [Data] { items }
}

private actor ApprovalLog {
    private var decisions: [String] = []
    private var script: [PikraToolApproval.Decision]

    init(script: [PikraToolApproval.Decision]) {
        self.script = script
    }

    func next(for request: PikraToolApproval.Request) -> PikraToolApproval.Decision {
        decisions.append("\(request.pluginID):\(request.toolName):\(request.effect)")
        if script.isEmpty { return .deny }
        return script.removeFirst()
    }

    func recorded() -> [String] { decisions }
}

private func collectEvents(_ stream: AsyncThrowingStream<PikraEvent, any Error>) async throws -> [PikraEvent] {
    var events: [PikraEvent] = []
    for try await ev in stream {
        events.append(ev)
    }
    return events
}

private func canonicalJSON(_ value: Any) throws -> Data {
    try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
}

private func testModel(
    id: String = "mock-model-v1",
    contextWindow: UInt32 = 128_000,
    maxOutputTokens: UInt32 = 8_192
) -> PikraModel {
    PikraModel(
        id: id,
        name: id,
        provider: "mock",
        api: .openaiCompletions,
        baseURL: "https://mock.local/v1",
        contextWindow: contextWindow,
        maxOutputTokens: maxOutputTokens,
        supportsImages: true,
        supportsReasoning: true
    )
}

// MARK: - E2E Test Suite

@Suite("PikraKit End-to-End Tests")
struct PikraKitE2ETests {

    @Test("1. Multi-turn streaming, cache prefix stability, deferred tools, approval, codemode exec, JS extension & auto-compaction")
    func multiTurnFullStackE2E() async throws {
        // Verify default URLSession client is strictly ephemeral (zero persistent disk state).
        #expect(PikraURLSessionHTTPClient().permitsPersistentHTTPState == false)

        let beforeRequestBodies = DataCollector()
        let approvalLog = ApprovalLog(script: [
            .deny,        // 1st write_note call denied
            .allowPlugin, // 2nd write_note call grants write plugin
            .allowOnce,   // dangerous_op always prompts and is not cached
            .allowOnce,   // write_note after revoke prompts again
        ])
        let approval = PikraToolApproval { request in
            await approvalLog.next(for: request)
        }

        let readTool = approval.wrap(
            tool: PikraTool(
                name: "read_val",
                description: "Reads a numeric value by key",
                parameters: [
                    "type": "object",
                    "properties": ["key": ["type": "string"]],
                    "required": ["key"],
                ],
                deferred: false
            ) { ctx, args in
                ctx.reportProgress(.text("reading..."))
                let key = args["key"]?.stringValue ?? "missing"
                return .json(["key": .string(key), "value": 21])
            },
            pluginID: "workspace",
            effect: .readOnly
        )

        let writeTool = approval.wrap(
            tool: PikraTool(
                name: "write_note",
                description: "Writes a note",
                parameters: [
                    "type": "object",
                    "properties": ["note": ["type": "string"]],
                    "required": ["note"],
                ],
                deferred: false
            ) { _, args in
                let note = args["note"]?.stringValue ?? ""
                return .text("wrote:\(note)")
            },
            pluginID: "workspace",
            effect: .write
        )

        let dangerousTool = approval.wrap(
            tool: PikraTool(
                name: "danger_op",
                description: "Dangerous operation",
                parameters: [
                    "type": "object",
                    "properties": ["target": ["type": "string"]],
                    "required": ["target"],
                ],
                deferred: false
            ) { _, args in
                .text("ran-danger:\(args["target"]?.stringValue ?? "")")
            },
            pluginID: "workspace",
            effect: .dangerous
        )

        let deferredMathTool = PikraTool(
            name: "multiply_deferred",
            description: "Multiplies two integers in a deferred tool",
            parameters: [
                "type": "object",
                "properties": [
                    "a": ["type": "integer"],
                    "b": ["type": "integer"],
                ],
                "required": ["a", "b"],
            ],
            deferred: true
        ) { _, args in
            let a = args["a"]?.intValue ?? 0
            let b = args["b"]?.intValue ?? 0
            return .json(["product": .number(Double(a * b))])
        }

        // Scripted responses across turns:
        // Req 0 (Turn 1): plain text + thinking
        // Req 1 (Turn 2): call write_note (denied) -> Req 2: call write_note (allowPlugin) -> Req 3: call write_note again (cached grant!) -> Req 4: call danger_op (always prompts) -> Req 5: final text
        // Req 6 (Turn 3 after upsertTools): call tool_search -> Req 7: call_tool(multiply_deferred) -> Req 8: call exec(codemode calling read_val + multiply_deferred + js_double) -> Req 9: final text with high usage triggering auto-compaction!
        // Req 10 (Auto-compaction summarization): summary text
        // Req 11 (Turn 4 after compaction + revoke): call write_note (re-prompts after revoke!) -> Req 12: final text
        let mockClient = MockSSEClient(handlers: [
            // Req 0: Turn 1
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Hello Turn 1", thinking: "pondering")) },
            // Req 1: Turn 2 step 1 -> write_note (will be denied)
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_w1", name: "write_note", argumentsJSON: "{\"note\":\"first\"}")) },
            // Req 2: Turn 2 step 2 -> write_note (will be allowPlugin)
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_w2", name: "write_note", argumentsJSON: "{\"note\":\"second\"}")) },
            // Req 3: Turn 2 step 3 -> write_note (uses cached write grant, no prompt!)
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_w3", name: "write_note", argumentsJSON: "{\"note\":\"third\"}")) },
            // Req 4: Turn 2 step 4 -> danger_op (dangerous always prompts even with write grant)
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_d1", name: "danger_op", argumentsJSON: "{\"target\":\"prod\"}")) },
            // Req 5: Turn 2 done
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Approval checks complete")) },
            // Req 6: Turn 3 step 1 -> tool_search for deferred tool
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_s1", name: "tool_search", argumentsJSON: "{\"query\":\"multiply\"}")) },
            // Req 7: Turn 3 step 2 -> direct call_tool on deferred tool
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_c1", name: "call_tool", argumentsJSON: "{\"name\":\"multiply_deferred\",\"arguments\":{\"a\":6,\"b\":7}}")) },
            // Req 8: Turn 3 step 3 -> codemode exec calling resident + deferred + JS extension tool
            { _, _ in
                let code = """
                const r = await tools.read_val({ key: "alpha" });
                const m = await tools.multiply_deferred({ a: r.value, b: 2 });
                const j = await tools.js_double({ n: m.product });
                return { read: r.value, mult: m.product, doubled: j };
                """
                let args = try String(data: JSONSerialization.data(withJSONObject: ["code": code]), encoding: .utf8)!
                return MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_exec", name: "exec", argumentsJSON: args))
            },
            // Req 9: Turn 3 final text with prompt_tokens=1450 (> 1500 - 200) -> triggers auto-compaction!
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Codemode done: 84", promptTokens: 1450, completionTokens: 20)) },
            // Req 10: Auto-compaction summary response
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "## Goal\nVerified full stack.\n## Progress\n### Done\n- All checks passed", promptTokens: 200, completionTokens: 30)) },
            // Req 11: Turn 4 after compaction & revoke -> write_note prompts again
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_w4", name: "write_note", argumentsJSON: "{\"note\":\"after_revoke\"}")) },
            // Req 12: Turn 4 done
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Post-compaction turn done")) },
        ])

        let jsExt = PikraJsExtensionConfig(sources: [
            .inline(
                name: "double.js",
                code: """
                pikra.registerTool({
                    name: "js_double",
                    description: "Doubles an integer in QuickJS",
                    parameters: { type: "object", properties: { n: { type: "integer" } }, required: ["n"] },
                    execute: async (_id, args) => String((args.n || 0) * 2)
                });
                """
            )
        ])

        let config = PikraConfiguration(
            model: testModel(contextWindow: 1_500, maxOutputTokens: 200),
            systemPrompt: "You are Pikra test agent.",
            tools: [readTool, writeTool, dangerousTool],
            enableDeferredGateway: true,
            enableCodemode: true,
            compaction: PikraCompactionConfig(enabled: true, reserveTokens: 200, keepRecentTokens: 10),
            jsExtensions: jsExt,
            httpClient: mockClient,
            beforeRequest: { body in
                await beforeRequestBodies.append(body)
            }
        )

        let agent = try await PikraAgent.open(configuration: config)

        // Set named context before Turn 1
        let changed1 = try await agent.setContext(name: "env", text: "region=us-west")
        #expect(changed1 == true)

        let turn1Events = try await collectEvents(try await agent.prompt("Turn 1"))
        #expect(turn1Events.contains { if case .contextAppended(let n, _) = $0 { return n == "env" }; return false })
        #expect(turn1Events.contains { if case .thinkingDelta(_, let d) = $0 { return d == "pondering" }; return false })
        #expect(turn1Events.contains { if case .requestUsage = $0 { return true }; return false })
        #expect(turn1Events.last == .turnEnded(stopReason: .stop, userMessageKept: true, errorMessage: nil))

        // Duplicate setContext MUST be a no-op (preserving prompt cache prefix!)
        let changedDup = try await agent.setContext(name: "env", text: "region=us-west")
        #expect(changedDup == false)

        let turn2Events = try await collectEvents(try await agent.prompt("Turn 2"))
        // Verify no duplicate contextAppended on Turn 2
        #expect(!turn2Events.contains { if case .contextAppended = $0 { return true }; return false })
        // Verify approval deny + allowPlugin + cached grant + dangerous prompt
        let toolFinishesT2 = turn2Events.compactMap { ev -> (String, Bool, String)? in
            if case .toolFinished(_, let name, let res, let isErr) = ev {
                return (name, isErr, res.textOutput)
            }
            return nil
        }
        #expect(toolFinishesT2.count == 4)
        #expect(toolFinishesT2[0].0 == "write_note" && toolFinishesT2[0].1 == true) // denied
        #expect(toolFinishesT2[1].0 == "write_note" && toolFinishesT2[1].1 == false && toolFinishesT2[1].2 == "wrote:second")
        #expect(toolFinishesT2[2].0 == "write_note" && toolFinishesT2[2].1 == false && toolFinishesT2[2].2 == "wrote:third")
        #expect(toolFinishesT2[3].0 == "danger_op" && toolFinishesT2[3].1 == false && toolFinishesT2[3].2 == "ran-danger:prod")
        #expect(await approval.allowedPlugins == ["workspace"])

        // Dynamically upsert deferred tool while idle — resident `tools` MUST remain byte-identical!
        try await agent.upsertTools([deferredMathTool])

        let turn3Events = try await collectEvents(try await agent.prompt("Turn 3"))
        #expect(turn3Events.contains { if case .contextAppended(let n, _) = $0 { return n == "deferred_tools" }; return false })
        // Verify tool_search, call_tool, and exec codemode results
        let execFinish = turn3Events.compactMap { ev -> String? in
            if case .toolFinished(_, "exec", let res, let isErr) = ev, !isErr {
                return res.textOutput
            }
            return nil
        }.first
        #expect(execFinish?.contains("84") == true)
        #expect(execFinish?.contains("42") == true)
        // Verify auto-compaction fired inside Turn 3!
        #expect(turn3Events.contains { if case .compactionStarted = $0 { return true }; return false })
        #expect(turn3Events.contains { if case .compactionEnded = $0 { return true }; return false })

        // Revoke write grant and run Turn 4 -> verifies named context blocks are restored after compaction AND revoke re-prompts!
        await approval.revoke(pluginID: "workspace")
        #expect(await approval.allowedPlugins.isEmpty)

        let turn4Events = try await collectEvents(try await agent.prompt("Turn 4"))
        #expect(turn4Events.contains { if case .contextAppended(let n, let t) = $0 { return n == "env" && t == "region=us-west" }; return false })
        #expect(turn4Events.last == .turnEnded(stopReason: .stop, userMessageKept: true, errorMessage: nil))

        // Verify approval prompt sequence (3rd write_note skipped prompt; 4th re-prompted after revoke)
        let prompts = await approvalLog.recorded()
        #expect(prompts.count == 4)

        // Verify beforeRequest saw every request and resident `tools` + Turn 1->2 message prefix were byte-identical!
        let bodies = await beforeRequestBodies.all()
        #expect(bodies.count == 13)

        let req0 = try JSONSerialization.jsonObject(with: bodies[0]) as! [String: Any]
        let req1 = try JSONSerialization.jsonObject(with: bodies[1]) as! [String: Any]
        let req6 = try JSONSerialization.jsonObject(with: bodies[6]) as! [String: Any]

        // Exact byte-identical resident `tools` array across Turn 1, Turn 2, and Turn 3 (after upsertTools!)
        let tools0 = try canonicalJSON(req0["tools"]!)
        let tools1 = try canonicalJSON(req1["tools"]!)
        let tools6 = try canonicalJSON(req6["tools"]!)
        #expect(tools0 == tools1)
        #expect(tools0 == tools6)

        // Exact byte-identical message prefix between Turn 1 and Turn 2
        let msgs0 = req0["messages"] as! [[String: Any]]
        let msgs1 = req1["messages"] as! [[String: Any]]
        #expect(msgs1.count > msgs0.count)
        let prefix0 = try canonicalJSON(msgs0)
        let prefix1 = try canonicalJSON(Array(msgs1.prefix(msgs0.count)))
        #expect(prefix0 == prefix1)

        await agent.close()
    }

    @Test("2. Mid-turn steer + interrupt awaits Swift tool cancellation, drops unread steer, immediate setModel/setFast/prompt never busy")
    func steerAndInterruptTeardownE2E() async throws {
        let tool1Started = Signal()
        let tool1AllowFinish = Signal()
        let tool2Started = Signal()
        let tool2Cancelled = Signal()

        let step1Tool = PikraTool(
            name: "step_one",
            description: "First tool during which steer #1 arrives",
            parameters: ["type": "object", "properties": [:]]
        ) { _, _ in
            await tool1Started.fire()
            await tool1AllowFinish.wait()
            return .text("step1-done")
        }

        let slowTool = PikraTool(
            name: "slow_cancellable",
            description: "Slow tool that waits until cancelled by interrupt()",
            parameters: ["type": "object", "properties": [:]]
        ) { _, _ in
            await tool2Started.fire()
            let gate = CancellableGate()
            do {
                try await gate.waitUntilCancelled()
                return .text("unreachable")
            } catch {
                await tool2Cancelled.fire()
                throw error
            }
        }

        let mockClient = MockSSEClient(handlers: [
            // Req 0: Turn 1 invokes step_one
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_s1", name: "step_one", argumentsJSON: "{}")) },
            // Req 1: After steer #1 is delivered, invokes slow_cancellable
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "tc_s2", name: "slow_cancellable", argumentsJSON: "{}")) },
            // Req 2: Next prompt after interrupt + setModel + setFast(true)
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Fresh turn after interrupt!")) },
        ])

        let agent = try await PikraAgent.open(
            configuration: PikraConfiguration(
                model: testModel(id: "model-a"),
                systemPrompt: "Interrupt test",
                tools: [step1Tool, slowTool],
                httpClient: mockClient
            )
        )

        // Idle steer must return false
        #expect(await agent.steer(text: "ignored-while-idle", id: "idle-1") == false)

        let stream = try await agent.prompt("Start work")
        let collectorTask = Task {
            try await collectEvents(stream)
        }

        // Wait until step_one is actively running, then queue steer #1 (will be consumed!)
        await tool1Started.wait()
        let acceptedSteer1 = await agent.steer(text: "Now call slow_cancellable", id: "steer-1")
        #expect(acceptedSteer1 == true)
        await tool1AllowFinish.fire()

        // Wait until slow_cancellable starts, queue steer #2 (unread — must be dropped on interrupt!), then interrupt()
        await tool2Started.wait()
        let acceptedSteer2 = await agent.steer(text: "Unread steer should be dropped", id: "steer-2")
        #expect(acceptedSteer2 == true)

        // interrupt() MUST wait until slow_cancellable observes cancellation and completes teardown!
        await agent.interrupt()
        await tool2Cancelled.wait()

        // Immediately mutate model + fast + start next prompt with ZERO `busy` error!
        try await agent.setModel(testModel(id: "model-b"))
        try await agent.setFast(true)
        try await agent.setReasoningEffort(.medium)

        #expect(try await agent.currentModel().id == "model-b")
        #expect(try await agent.isFast() == true)
        #expect(try await agent.currentReasoningEffort() == .medium)

        let nextEvents = try await collectEvents(try await agent.prompt("After interrupt"))
        #expect(nextEvents.last == .turnEnded(stopReason: .stop, userMessageKept: true, errorMessage: nil))

        let firstTurnEvents = try await collectorTask.value
        let steeredIDs = firstTurnEvents.compactMap { ev -> String? in
            if case .steered(let id, _) = ev { return id }
            return nil
        }
        #expect(steeredIDs == ["steer-1"]) // steer-1 consumed; steer-2 dropped unread!

        // Verify the third HTTP request used the switched model `model-b`
        let reqs = await mockClient.recordedRequests()
        #expect(reqs.count == 3)
        let req2Obj = try JSONSerialization.jsonObject(with: reqs[2].body) as! [String: Any]
        #expect(req2Obj["model"] as? String == "model-b")

        await agent.close()
    }

    @Test("3. JSONL session persistence, reopen & fork across PikraAgent instances")
    func jsonlPersistenceReopenE2E() async throws {
        let tmpDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("pikra-swift-e2e-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tmpDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmpDir) }

        let journalURL = tmpDir.appendingPathComponent("session.jsonl")

        // Instance 1: write two turns
        let client1 = MockSSEClient(handlers: [
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Remembered secret: PINEAPPLE-42")) },
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Second turn acknowledged")) },
        ])
        let agent1 = try await PikraAgent.open(
            configuration: PikraConfiguration(
                model: testModel(),
                systemPrompt: "Persistent session test",
                persistenceURL: journalURL,
                httpClient: client1
            )
        )
        let sessionID1 = try await agent1.sessionID()
        _ = try await collectEvents(try await agent1.prompt("Remember PINEAPPLE-42"))
        let leafAfterTurn1 = try await agent1.leafID()
        _ = try await collectEvents(try await agent1.prompt("Second message"))
        let entries1 = try await agent1.entries()
        #expect(entries1.count >= 5) // baseline + 2 user + 2 assistant
        await agent1.close()

        // Instance 2: reopen same JSONL file and verify history is sent on wire + fork works
        let client2 = MockSSEClient(handlers: [
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Confirmed PINEAPPLE-42")) },
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Forked branch reply")) },
        ])
        let agent2 = try await PikraAgent.open(
            configuration: PikraConfiguration(
                model: testModel(),
                systemPrompt: "Different prompt ignored because baseline is persisted",
                persistenceURL: journalURL,
                httpClient: client2
            )
        )
        #expect(try await agent2.sessionID() == sessionID1)

        _ = try await collectEvents(try await agent2.prompt("What was the secret?"))
        let reqs2 = await client2.recordedRequests()
        let wireBody = String(data: reqs2[0].body, encoding: .utf8) ?? ""
        #expect(wireBody.contains("PINEAPPLE-42"))
        #expect(wireBody.contains("Persistent session test"))

        // Fork back to Turn 1 leaf and send a branched prompt
        let forkTarget = try #require(leafAfterTurn1)
        try await agent2.fork(entryID: forkTarget)
        _ = try await collectEvents(try await agent2.prompt("Branched prompt"))
        let reqsAfterFork = await client2.recordedRequests()
        let forkedWire = String(data: reqsAfterFork[1].body, encoding: .utf8) ?? ""
        #expect(forkedWire.contains("PINEAPPLE-42"))
        #expect(!forkedWire.contains("Second message"))

        await agent2.close()
    }

    @Test("4. Optional Live DeepSeek streaming + tool call E2E over PikraURLSessionHTTPClient")
    func liveDeepSeekE2E() async throws {
        guard let apiKey = ProcessInfo.processInfo.environment["DORA_TEST_DEEPSEEK_KEY"],
              !apiKey.isEmpty
        else {
            return
        }

        let liveClient = PikraURLSessionHTTPClient()
        #expect(liveClient.permitsPersistentHTTPState == false)

        let addTool = PikraTool(
            name: "add_numbers",
            description: "Adds two integers a and b and returns their sum.",
            parameters: [
                "type": "object",
                "properties": [
                    "a": ["type": "integer"],
                    "b": ["type": "integer"],
                ],
                "required": ["a", "b"],
            ]
        ) { _, args in
            let a = args["a"]?.intValue ?? 0
            let b = args["b"]?.intValue ?? 0
            return .text("\(a + b)")
        }

        let model = PikraModel(
            id: "deepseek-chat",
            name: "DeepSeek Chat",
            provider: "deepseek",
            api: .openaiCompletions,
            baseURL: "https://api.deepseek.com/v1",
            contextWindow: 64_000,
            maxOutputTokens: 4_096,
            supportsImages: false,
            supportsReasoning: false
        )

        let agent = try await PikraAgent.open(
            configuration: PikraConfiguration(
                model: model,
                systemPrompt: "Always call the add_numbers tool when asked to add numbers, then answer concisely with the result.",
                tools: [addTool],
                apiKey: apiKey,
                temperature: 0,
                maxTokens: 256,
                enableDeferredGateway: false,
                httpClient: liveClient
            )
        )

        let events = try await collectEvents(
            try await agent.prompt("Use add_numbers to compute 17 + 25 and tell me the answer.")
        )

        let streamedText = events.compactMap { ev -> String? in
            if case .textDelta(_, let delta) = ev { return delta }
            return nil
        }.joined()
        let calledTool = events.contains { ev in
            if case .toolFinished(_, "add_numbers", let res, false) = ev {
                return res.textOutput == "42"
            }
            return false
        }
        let sawUsage = events.contains { ev in
            if case .requestUsage(_, _, let u) = ev { return u.totalTokens > 0 }
            return false
        }

        #expect(calledTool)
        #expect(sawUsage)
        #expect(streamedText.contains("42"))
        #expect(events.last == .turnEnded(stopReason: .stop, userMessageKept: true, errorMessage: nil))

        await agent.close()
    }
}
