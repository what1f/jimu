import CryptoKit
import Darwin
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
            id: "deepseek-flash",
            name: "DeepSeek Flash",
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

    @Test("5. Codex OAuth PKCE, JWT claims, coalesced PikraCodexSubscription refresh & PikraAgent authorizeRequest")
    func codexOAuthAndAuthorizeRequestE2E() async throws {
        // Verify PikraThinkingLevel.none & PikraApiKind.openaiCodexResponses wire encoding.
        #expect(PikraThinkingLevel.off.rawValue == "off")
        #expect(PikraThinkingLevel.none.rawValue == "none")
        #expect(PikraApiKind.openaiCodexResponses.rawValue == "openai-codex-responses")
        #expect(PikraApiKind(rawValue: "openai-codex-responses") == .openaiCodexResponses)

        func makeJWT(claims: [String: Any]) throws -> String {
            func b64url(_ data: Data) -> String {
                data.base64EncodedString()
                    .replacingOccurrences(of: "+", with: "-")
                    .replacingOccurrences(of: "/", with: "_")
                    .replacingOccurrences(of: "=", with: "")
            }
            let header = try JSONSerialization.data(withJSONObject: ["alg": "none", "typ": "JWT"])
            let payload = try JSONSerialization.data(withJSONObject: claims)
            return "\(b64url(header)).\(b64url(payload)).sig"
        }

        let initialExp = Date().addingTimeInterval(60).timeIntervalSince1970 // within 300s leeway -> triggers refresh
        let refreshedExp = Date().addingTimeInterval(3600).timeIntervalSince1970
        let initialJWT = try makeJWT(claims: [
            "exp": initialExp,
            "https://api.openai.com/auth": ["chatgpt_account_id": "acct_initial_123"],
            "https://api.openai.com/profile": ["email": "alice@example.com"],
        ])
        let refreshedJWT = try makeJWT(claims: [
            "exp": refreshedExp,
            "email": "alice@example.com",
        ]) // intentionally omits chatgpt_account_id to test preservation on refresh

        let refreshStarted = Signal()
        let allowRefreshFinish = Signal()

        let oauthMockClient = MockSSEClient(handlers: [
            // Req 0: exchange authorization code (full URL callback)
            { req, _ in
                let bodyStr = String(data: req.body, encoding: .utf8) ?? ""
                #expect(bodyStr.contains("grant_type=authorization_code"))
                #expect(bodyStr.contains("code=auth_code_url"))
                let json = "{\"access_token\":\"\(initialJWT)\",\"refresh_token\":\"rt_initial\"}"
                return MockSSEClient.sseResponse(chunks: [json])
            },
            // Req 1: exchange authorization code (code#state format)
            { req, _ in
                let bodyStr = String(data: req.body, encoding: .utf8) ?? ""
                #expect(bodyStr.contains("code=auth_code_hash"))
                let json = "{\"access_token\":\"\(initialJWT)\",\"refresh_token\":\"rt_initial\"}"
                return MockSSEClient.sseResponse(chunks: [json])
            },
            // Req 2: coalesced token refresh (omits refresh_token & chatgpt_account_id)
            { req, _ in
                let bodyStr = String(data: req.body, encoding: .utf8) ?? ""
                #expect(bodyStr.contains("grant_type=refresh_token"))
                #expect(bodyStr.contains("refresh_token=rt_initial"))
                await refreshStarted.fire()
                await allowRefreshFinish.wait()
                let json = "{\"access_token\":\"\(refreshedJWT)\"}"
                return MockSSEClient.sseResponse(chunks: [json])
            },
        ])

        let oauth = PikraCodexOAuth(httpClient: oauthMockClient)
        let auth = try oauth.startAuthorization()
        let components = try #require(URLComponents(url: auth.url, resolvingAgainstBaseURL: false))
        let queryMap = Dictionary(
            uniqueKeysWithValues: (components.queryItems ?? []).compactMap { item -> (String, String)? in
                guard let v = item.value else { return nil }
                return (item.name, v)
            }
        )
        #expect(queryMap["client_id"] == PikraCodexOAuth.clientID)
        #expect(queryMap["response_type"] == "code")
        #expect(queryMap["code_challenge_method"] == "S256")
        #expect(queryMap["state"] == auth.state)
        #expect(queryMap["originator"] == "pikra")
        #expect(queryMap["codex_cli_simplified_flow"] == "true")

        // Verify S256 PKCE challenge matches verifier
        let expectedChallenge = Data(SHA256.hash(data: Data(auth.codeVerifier.utf8)))
            .base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        #expect(queryMap["code_challenge"] == expectedChallenge)

        // State mismatch or missing state in callback URL must throw
        await #expect(throws: PikraCodexOAuthError.stateMismatch) {
            _ = try await oauth.completeAuthorization(auth, callback: "http://localhost:1455/auth/callback?code=abc&state=wrong")
        }
        await #expect(throws: PikraCodexOAuthError.stateMismatch) {
            _ = try await oauth.completeAuthorization(auth, callback: "http://localhost:1455/auth/callback?code=abc")
        }

        // Complete via full callback URL
        let exchanged = try await oauth.completeAuthorization(
            auth,
            callback: "http://localhost:1455/auth/callback?code=auth_code_url&state=\(auth.state)"
        )
        #expect(exchanged.accessToken == initialJWT)
        #expect(exchanged.refreshToken == "rt_initial")
        #expect(exchanged.accountID == "acct_initial_123")
        #expect(exchanged.email == "alice@example.com")
        #expect(exchanged.expiresAt != nil)

        // Complete via code#state format
        let exchangedHash = try await oauth.completeAuthorization(auth, callback: "auth_code_hash#\(auth.state)")
        #expect(exchangedHash.accountID == "acct_initial_123")

        // Coalesce concurrent refreshes through PikraCodexSubscription (persisting refreshed credentials in store)
        actor SavedCreds {
            var current: PikraCodexCredentials
            var list: [PikraCodexCredentials] = []
            init(initial: PikraCodexCredentials) { self.current = initial }
            func load() -> PikraCodexCredentials { current }
            func append(_ c: PikraCodexCredentials) {
                current = c
                list.append(c)
            }
            func all() -> [PikraCodexCredentials] { list }
        }
        let savedCreds = SavedCreds(initial: exchanged)
        let subscription = PikraCodexSubscription(
            credentials: { await savedCreds.load() },
            onRefresh: { updated in await savedCreds.append(updated) },
            httpClient: oauthMockClient
        )

        let task1 = Task { try await subscription.currentCredentials() }
        await refreshStarted.wait()
        let task2 = Task { try await subscription.currentCredentials() }
        await allowRefreshFinish.fire()

        let c1 = try await task1.value
        let c2 = try await task2.value
        #expect(c1 == c2)
        #expect(c1.accessToken == refreshedJWT)
        #expect(c1.refreshToken == "rt_initial")
        #expect(c1.accountID == "acct_initial_123")
        #expect(await savedCreds.all().count == 1)
        #expect(await oauthMockClient.recordedRequests().count == 3)

        // Wire subscription.requestAuthorizer into PikraAgent (Codex endpoint) and verify dynamic headers + cross-origin protection
        let codexChunks = [
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r_codex_1\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"m_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"Authorized ok\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r_codex_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":4,\"total_tokens\":16}}}\n\n",
        ]
        let llmClient = MockSSEClient(handlers: [
            { _, _ in MockSSEClient.sseResponse(chunks: codexChunks) },
        ])
        let codexModel = PikraModel(
            id: "gpt-5.4",
            name: "GPT-5.4",
            provider: "openai-codex",
            api: .openaiCodexResponses,
            baseURL: PikraCodexOAuth.codexBaseURL,
            contextWindow: 128_000,
            maxOutputTokens: 8_192,
            supportsImages: true,
            supportsReasoning: true
        )
        let agent = try await PikraAgent.open(
            configuration: PikraConfiguration(
                model: codexModel,
                systemPrompt: "Auth test",
                httpClient: llmClient,
                authorizeRequest: subscription.requestAuthorizer
            )
        )
        let events = try await collectEvents(try await agent.prompt("Ping"))
        #expect(events.last == .turnEnded(stopReason: .stop, userMessageKept: true, errorMessage: nil))

        let llmReqs = await llmClient.recordedRequests()
        #expect(llmReqs.count == 1)
        let headerMap = Dictionary(
            llmReqs[0].headers.map { ($0.0.lowercased(), $0.1) },
            uniquingKeysWith: { _, latest in latest }
        )
        #expect(headerMap["authorization"] == "Bearer \(refreshedJWT)")
        #expect(headerMap["chatgpt-account-id"] == "acct_initial_123")
        #expect(headerMap["openai-beta"] == "responses=experimental")
        #expect(headerMap["originator"] == "pikra")

        // Switching to a non-Codex host MUST reject attaching ChatGPT bearer tokens before hitting network
        try await agent.setModel(testModel())
        let blockedEvents = try await collectEvents(try await agent.prompt("Should not leak token"))
        if case .turnEnded(let reason, _, _) = blockedEvents.last {
            #expect(reason == .error)
        } else {
            Issue.record("Expected blocked cross-origin request to end in .error")
        }
        #expect(await llmClient.recordedRequests().count == 1)

        await agent.close()
    }

    @Test("6. Skill discovery via pikra_discover_skills & live PikraSkillCatalog directory watcher (zero Task.sleep)")
    func skillDiscoveryAndWatcherE2E() async throws {
        let tmpDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("pikra-skills-e2e-\(UUID().uuidString)", isDirectory: true)
        let alphaDir = tmpDir.appendingPathComponent("alpha", isDirectory: true)
        let hiddenDir = tmpDir.appendingPathComponent("hidden", isDirectory: true)
        try FileManager.default.createDirectory(at: alphaDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: hiddenDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmpDir) }

        let alphaSkill = alphaDir.appendingPathComponent("SKILL.md")
        let hiddenSkill = hiddenDir.appendingPathComponent("SKILL.md")
        try """
        ---
        name: alpha
        description: Alpha skill v1
        ---
        # Alpha
        Instructions v1.
        """.write(to: alphaSkill, atomically: true, encoding: .utf8)
        try """
        ---
        name: hidden
        description: Manual only skill
        disable-model-invocation: true
        ---
        # Hidden
        """.write(to: hiddenSkill, atomically: true, encoding: .utf8)

        let discovered = try PikraSkillDiscovery.discover(roots: [tmpDir])
        #expect(discovered.skills.count == 2)
        let alphaEntry = try #require(discovered.skills.first { $0.name == "alpha" })
        let hiddenEntry = try #require(discovered.skills.first { $0.name == "hidden" })
        #expect(alphaEntry.description == "Alpha skill v1")
        #expect(alphaEntry.disableModelInvocation == false)
        #expect(alphaEntry.revision.count == 16)
        #expect(hiddenEntry.disableModelInvocation == true)
        #expect(discovered.prompt.hasPrefix("Available skills. Use read to load a skill's SKILL.md"))
        #expect(discovered.prompt.contains("<available_skills>"))
        #expect(discovered.prompt.contains("<name>alpha</name>"))
        #expect(discovered.prompt.contains("<revision>\(alphaEntry.revision)</revision>"))
        #expect(!discovered.prompt.contains("<name>hidden</name>"))

        // Empty root produces empty prompt
        let emptyDir = tmpDir.appendingPathComponent("empty", isDirectory: true)
        try FileManager.default.createDirectory(at: emptyDir, withIntermediateDirectories: true)
        let emptyCatalog = try PikraSkillDiscovery.discover(roots: [emptyDir])
        #expect(emptyCatalog.skills.isEmpty)
        #expect(emptyCatalog.prompt.isEmpty)

        // Live event-driven watcher (zero Task.sleep)
        let catalog = PikraSkillCatalog(roots: [tmpDir])
        let updateTask = Task { () -> Bool in
            var iter = catalog.updates.makeAsyncIterator()
            return await iter.next() != nil
        }

        // Modify SKILL.md and add a new skill directory to trigger kqueue watcher immediately
        try """
        ---
        name: alpha
        description: Alpha skill v2
        ---
        # Alpha
        Instructions v2.
        """.write(to: alphaSkill, atomically: true, encoding: .utf8)

        let sawUpdate = await updateTask.value
        #expect(sawUpdate == true)

        let refreshed = try await catalog.discover()
        let updatedAlpha = try #require(refreshed.skills.first { $0.name == "alpha" })
        #expect(updatedAlpha.description == "Alpha skill v2")
        #expect(updatedAlpha.revision != alphaEntry.revision)

        await catalog.close()
    }

    @Test("7. PikraWorkspaceTools (read, write, edit, find, bash) with PikraToolApproval E2E")
    func workspaceToolsE2E() async throws {
        let workspace = FileManager.default.temporaryDirectory
            .appendingPathComponent("pikra-ws-e2e-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: workspace) }

        // Seed an ignored .git file and a minimal valid 1x1 PNG
        let gitDir = workspace.appendingPathComponent(".git", isDirectory: true)
        try FileManager.default.createDirectory(at: gitDir, withIntermediateDirectories: true)
        try "ignored".write(to: gitDir.appendingPathComponent("config"), atomically: true, encoding: .utf8)
        let pngBytes = Data([
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
            0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41,
            0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
            0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
            0x42, 0x60, 0x82,
        ])
        let pngURL = workspace.appendingPathComponent("pixel.png")
        try pngBytes.write(to: pngURL)

        let approvalLog = ApprovalLog(script: [
            .allowPlugin, // write -> grants plugin write access so edit skips prompting
            .allowOnce,   // bash #1 (dangerous -> always prompts)
            .allowOnce,   // bash #2 (dangerous -> always prompts)
        ])
        let approval = PikraToolApproval { req in
            await approvalLog.next(for: req)
        }

        let wsTools = PikraWorkspaceTools(
            workspace: workspace,
            approval: approval,
            pluginID: "ws-plugin",
            environment: { ["PIKRA_WS_ENV": "env_ok"] },
            shellOutputLimit: 32
        )
        let tools = wsTools.tools([.readFile, .writeFile, .editFile, .findFiles, .shell])

        let mockClient = MockSSEClient(handlers: [
            // 1. write nested file
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(
                    id: "tc_1",
                    name: "write",
                    argumentsJSON: "{\"path\":\"sub/notes.txt\",\"content\":\"line1\\nline2\\nline3\"}"
                ))
            },
            // 2. edit unique line (uses cached write grant!)
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(
                    id: "tc_2",
                    name: "edit",
                    argumentsJSON: "{\"path\":\"sub/notes.txt\",\"old_text\":\"line2\",\"new_text\":\"LINE_TWO\"}"
                ))
            },
            // 3. read with offset & limit (readOnly -> no prompt)
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(
                    id: "tc_3",
                    name: "read",
                    argumentsJSON: "{\"path\":\"sub/notes.txt\",\"offset\":2,\"limit\":2}"
                ))
            },
            // 4. read PNG image (readOnly -> returns image content part)
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(
                    id: "tc_4",
                    name: "read",
                    argumentsJSON: "{\"path\":\"pixel.png\"}"
                ))
            },
            // 5. find files (skips .git)
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(
                    id: "tc_5",
                    name: "find",
                    argumentsJSON: "{\"pattern\":\"*.txt\"}"
                ))
            },
            // 6. bash success with custom env + truncation
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(
                    id: "tc_6",
                    name: "bash",
                    argumentsJSON: "{\"command\":\"printf '%s:%s' \\\"$PIKRA_WS_ENV\\\" \\\"0123456789abcdefghijklmnopqrstuvwxyz\\\"\"}"
                ))
            },
            // 7. bash non-zero exit
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(
                    id: "tc_7",
                    name: "bash",
                    argumentsJSON: "{\"command\":\"echo boom && exit 7\"}"
                ))
            },
            // 8. final response
            { _, _ in
                MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Workspace tools verified"))
            },
        ])

        let agent = try await PikraAgent.open(
            configuration: PikraConfiguration(
                model: testModel(),
                systemPrompt: "Workspace tool test",
                tools: tools,
                enableDeferredGateway: false,
                httpClient: mockClient
            )
        )

        let events = try await collectEvents(try await agent.prompt("Run workspace tools"))
        #expect(events.last == .turnEnded(stopReason: .stop, userMessageKept: true, errorMessage: nil))

        let toolResults = events.compactMap { ev -> (String, String, PikraToolOutput, Bool)? in
            if case .toolFinished(let id, let name, let res, let isErr) = ev {
                return (id, name, res, isErr)
            }
            return nil
        }
        #expect(toolResults.count == 7)

        // 1. write created file with 0600 permissions
        #expect(toolResults[0].1 == "write" && toolResults[0].3 == false)
        var st = Darwin.stat()
        #expect(stat(workspace.appendingPathComponent("sub/notes.txt").path, &st) == 0)
        #expect((st.st_mode & 0o777) == 0o600)

        // 2. edit succeeded without re-prompting
        #expect(toolResults[1].1 == "edit" && toolResults[1].3 == false)

        // 3. read offset=2, limit=2
        #expect(toolResults[2].1 == "read" && toolResults[2].3 == false)
        #expect(toolResults[2].2.textOutput == "LINE_TWO\nline3")

        // 4. read image returns PikraContentPart.image
        #expect(toolResults[3].1 == "read" && toolResults[3].3 == false)
        #expect(toolResults[3].2.content == [.image(data: pngBytes.base64EncodedString(), mimeType: "image/png")])

        // 5. find matched sub/notes.txt and skipped .git
        #expect(toolResults[4].1 == "find" && toolResults[4].3 == false)
        #expect(toolResults[4].2.textOutput == "sub/notes.txt")

        // 6. bash success with env & output_truncated
        #expect(toolResults[5].1 == "bash" && toolResults[5].3 == false)
        let bashOk = try JSONDecoder().decode(JSONValue.self, from: Data(toolResults[5].2.textOutput.utf8))
        #expect(bashOk["exit_code"]?.intValue == 0)
        #expect(bashOk["output_truncated"]?.boolValue == true)
        #expect(bashOk["output"]?.stringValue?.hasPrefix("env_ok:") == true)
        #expect((bashOk["output"]?.stringValue?.utf8.count ?? 0) <= 32)

        // 7. bash non-zero exit sets isError = true
        #expect(toolResults[6].1 == "bash" && toolResults[6].3 == true)
        let bashErr = try JSONDecoder().decode(JSONValue.self, from: Data(toolResults[6].2.textOutput.utf8))
        #expect(bashErr["exit_code"]?.intValue == 7)
        #expect(bashErr["output"]?.stringValue?.contains("boom") == true)

        // Approval sequence: write (allowPlugin), bash #1 (allowOnce), bash #2 (allowOnce); read/find/edit skipped
        let recordedApprovals = await approvalLog.recorded()
        #expect(recordedApprovals == [
            "ws-plugin:write:write",
            "ws-plugin:bash:dangerous",
            "ws-plugin:bash:dangerous",
        ])

        await agent.close()
    }

    @Test("8. Review regressions: OAuth account-switch guard, lossless Int64 JSONValue, CRLF read, write cancellation, tool registry sync, skill removal & FFI persistence error")
    func reviewRegressionsE2E() async throws {
        // #35: Lossless Int64 round-trip in JSONValue (above IEEE-754 2^53 safe integer bound)
        let bigID: Int64 = 9_007_199_254_740_993
        let rawBigJSON = Data("{\"id\":\(bigID),\"flt\":1.5}".utf8)
        let decodedBig = try JSONDecoder().decode(JSONValue.self, from: rawBigJSON)
        #expect(decodedBig["id"] == .integer(bigID))
        #expect(decodedBig["id"]?.int64Value == bigID)
        #expect(decodedBig["id"]?.intValue == Int(bigID))
        #expect(decodedBig["flt"]?.doubleValue == 1.5)
        let reencodedBig = try JSONEncoder().encode(decodedBig)
        let roundTrippedBig = try JSONDecoder().decode(JSONValue.self, from: reencodedBig)
        #expect(roundTrippedBig["id"]?.int64Value == bigID)

        // #2: PikraCodexSubscription must not clobber Account B when Account A's refresh finishes late
        func makeExpJWT(accountID: String, expOffset: TimeInterval) throws -> String {
            func b64url(_ data: Data) -> String {
                data.base64EncodedString()
                    .replacingOccurrences(of: "+", with: "-")
                    .replacingOccurrences(of: "/", with: "_")
                    .replacingOccurrences(of: "=", with: "")
            }
            let header = try JSONSerialization.data(withJSONObject: ["alg": "none", "typ": "JWT"])
            let payload = try JSONSerialization.data(withJSONObject: [
                "exp": Date().addingTimeInterval(expOffset).timeIntervalSince1970,
                "https://api.openai.com/auth": ["chatgpt_account_id": accountID],
            ])
            return "\(b64url(header)).\(b64url(payload)).sig"
        }

        let acctAInitial = try PikraCodexCredentials(
            accessToken: makeExpJWT(accountID: "acct_A", expOffset: 30),
            refreshToken: "rt_A_old"
        ).normalized()
        let acctARefreshedJWT = try makeExpJWT(accountID: "acct_A", expOffset: 3600)
        let acctB = try PikraCodexCredentials(
            accessToken: makeExpJWT(accountID: "acct_B", expOffset: 3600),
            refreshToken: "rt_B"
        ).normalized()

        actor AccountStore {
            var active: PikraCodexCredentials
            var saved: [PikraCodexCredentials] = []
            init(_ initial: PikraCodexCredentials) { self.active = initial }
            func load() -> PikraCodexCredentials { active }
            func switchTo(_ next: PikraCodexCredentials) { active = next }
            func save(_ creds: PikraCodexCredentials) {
                active = creds
                saved.append(creds)
            }
            func history() -> [PikraCodexCredentials] { saved }
        }

        let store = AccountStore(acctAInitial)
        let refreshAStarted = Signal()
        let allowRefreshAFinish = Signal()
        let oauthClient = MockSSEClient(handlers: [
            { _, _ in
                await refreshAStarted.fire()
                await allowRefreshAFinish.wait()
                return MockSSEClient.sseResponse(chunks: [
                    "{\"access_token\":\"\(acctARefreshedJWT)\",\"refresh_token\":\"rt_A_new\"}"
                ])
            }
        ])
        let sub = PikraCodexSubscription(
            credentials: { await store.load() },
            onRefresh: { updated in await store.save(updated) },
            httpClient: oauthClient
        )

        let refreshATask = Task { try await sub.currentCredentials() }
        await refreshAStarted.wait()
        // Switch active account to B while A's refresh is suspended!
        await store.switchTo(acctB)
        let fetchedB = try await sub.currentCredentials()
        #expect(fetchedB.accountID == "acct_B")
        await allowRefreshAFinish.fire()
        _ = try await refreshATask.value
        // B must remain intact on disk and onRefresh(A) must NOT have been called!
        #expect(await store.load().accountID == "acct_B")
        #expect(await store.history().isEmpty)
        // Switching back to A retries saving A's pending rotated credentials!
        await store.switchTo(acctAInitial)
        let restoredA = try await sub.currentCredentials()
        #expect(restoredA.refreshToken == "rt_A_new")
        #expect(await store.load().refreshToken == "rt_A_new")

        // #36 & #37: CRLF read slicing + cancelled write must not execute
        let tmpDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("pikra-reg-e2e-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tmpDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmpDir) }

        let crlfFile = tmpDir.appendingPathComponent("crlf.txt")
        try Data("alpha\r\nbeta\r\ngamma".utf8).write(to: crlfFile)

        let wsTools = PikraWorkspaceTools(workspace: tmpDir)
        let readTool = try #require(wsTools.tools([.readFile]).first)
        let writeTool = try #require(wsTools.tools([.writeFile]).first)
        let dummyCtx = PikraToolCallContext(toolCallID: "c1", toolName: "read") { _ in }

        let line2 = try await readTool.execute(dummyCtx, ["path": "crlf.txt", "offset": 2, "limit": 1])
        #expect(line2.textOutput == "beta")
        let lines23 = try await readTool.execute(dummyCtx, ["path": "crlf.txt", "offset": 2, "limit": 2])
        #expect(lines23.textOutput == "beta\ngamma")

        let cancelledTarget = tmpDir.appendingPathComponent("should_not_exist.txt")
        let cancelledWriteTask = Task {
            try await writeTool.execute(dummyCtx, ["path": "should_not_exist.txt", "content": "nope"])
        }
        cancelledWriteTask.cancel()
        _ = try? await cancelledWriteTask.value
        #expect(!FileManager.default.fileExists(atPath: cancelledTarget.path))

        // #11 & #38 & #7: Resident tool collision/removal safety, skill deletion reload, and FFI persistence error
        let skillRoot = tmpDir.appendingPathComponent("skills", isDirectory: true)
        let oneSkillDir = skillRoot.appendingPathComponent("demo", isDirectory: true)
        try FileManager.default.createDirectory(at: oneSkillDir, withIntermediateDirectories: true)
        let skillFile = oneSkillDir.appendingPathComponent("SKILL.md")
        try "---\nname: demo\ndescription: Demo skill\n---\n# Demo\n".write(to: skillFile, atomically: true, encoding: .utf8)

        let journalURL = tmpDir.appendingPathComponent("ffi-reg.jsonl")
        let residentBusyStarted = Signal()
        let residentBusyRelease = Signal()

        let residentEcho = PikraTool(
            name: "res_echo",
            description: "Resident echo",
            parameters: ["type": "object", "properties": ["id": ["type": "integer"]], "required": ["id"]],
            deferred: false
        ) { _, args in
            let idVal = args["id"]?.int64Value ?? 0
            return .text("resident:\(idVal)")
        }
        let blockerTool = PikraTool(
            name: "blocker",
            description: "Blocks until released",
            parameters: ["type": "object", "properties": [:]],
            deferred: false
        ) { _, _ in
            await residentBusyStarted.fire()
            await residentBusyRelease.wait()
            return .text("unblocked")
        }
        let dynToolV1 = PikraTool(
            name: "dyn_tool",
            description: "Dynamic v1",
            parameters: ["type": "object", "properties": [:]],
            deferred: true
        ) { _, _ in .text("dyn:v1") }
        let dynToolV2 = PikraTool(
            name: "dyn_tool",
            description: "Dynamic v2",
            parameters: ["type": "object", "properties": [:]],
            deferred: true
        ) { _, _ in .text("dyn:v2") }

        let mockClient = MockSSEClient(handlers: [
            // Turn 1: call blocker (while running, test busy upsertTools rejection!) -> then call call_tool(dyn_tool) -> text
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "t1", name: "blocker", argumentsJSON: "{}")) },
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "t2", name: "call_tool", argumentsJSON: "{\"name\":\"dyn_tool\",\"arguments\":{}}")) },
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "t3", name: "res_echo", argumentsJSON: "{\"id\":\(bigID)}")) },
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Turn 1 done")) },
            // Turn 2 (after deleting skill & calling reloadSkills + removeTools(["res_echo"])): call res_echo again!
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.toolCallChunks(id: "t4", name: "res_echo", argumentsJSON: "{\"id\":\(bigID)}")) },
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Turn 2 done")) },
            // Turn 3 (read-only journal -> persistence error!)
            { _, _ in MockSSEClient.sseResponse(chunks: MockSSEClient.textChunks(text: "Should fail persistence")) },
        ])

        let agent = try await PikraAgent.open(
            configuration: PikraConfiguration(
                model: testModel(),
                systemPrompt: "Regression test",
                tools: [residentEcho, blockerTool, dynToolV1],
                skillRoots: [skillRoot],
                enableDeferredGateway: true,
                persistenceURL: journalURL,
                httpClient: mockClient
            )
        )

        // Resident collision in upsertTools must be rejected
        await #expect(throws: PikraError.self) {
            try await agent.upsertTools([residentEcho])
        }

        // Start Turn 1; while blocker is active, upsertTools([dynToolV2]) must fail with .busy AND NOT overwrite dynToolV1!
        let t1Stream = try await agent.prompt("Run turn 1")
        let t1Task = Task { try await collectEvents(t1Stream) }
        await residentBusyStarted.wait()
        await #expect(throws: PikraError.busy) {
            try await agent.upsertTools([dynToolV2])
        }
        await residentBusyRelease.fire()
        let t1Events = try await t1Task.value
        let dynResult = t1Events.compactMap { ev -> String? in
            if case .toolFinished(_, "call_tool", let res, false) = ev { return res.textOutput }
            return nil
        }.first
        #expect(dynResult == "dyn:v1")
        let bigEchoResult = t1Events.compactMap { ev -> String? in
            if case .toolFinished(_, "res_echo", let res, false) = ev { return res.textOutput }
            return nil
        }.first
        #expect(bigEchoResult == "resident:\(bigID)")

        // #11: removeTools(["res_echo"]) returns 0 and must NOT remove res_echo from Swift bridge!
        let removedRes = try await agent.removeTools(named: ["res_echo"])
        #expect(removedRes == 0)

        // #38: Delete last skill and reloadSkills() -> must report changed == true and emit empty <available_skills>
        try FileManager.default.removeItem(at: oneSkillDir)
        let skillChanged = try await agent.reloadSkills()
        #expect(skillChanged == true)

        let t2Events = try await collectEvents(try await agent.prompt("Run turn 2"))
        #expect(t2Events.contains { ev in
            if case .contextAppended(let name, let text) = ev {
                return name == "skills" && text == "<available_skills>\n</available_skills>"
            }
            return false
        })
        #expect(t2Events.contains { ev in
            if case .toolFinished(_, "res_echo", let res, false) = ev {
                return res.textOutput == "resident:\(bigID)"
            }
            return false
        })

        // #7: Make journal read-only so append_message fails during Turn 3 -> TurnEnded must report .error!
        try FileManager.default.setAttributes([.posixPermissions: 0o444], ofItemAtPath: journalURL.path)
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: journalURL.path)
        }
        let t3Events = try await collectEvents(try await agent.prompt("Run turn 3 unwritable"))
        if case .turnEnded(let reason, _, let errMsg) = t3Events.last {
            #expect(reason == .error)
            #expect(errMsg?.contains("failed to persist session message") == true)
        } else {
            Issue.record("Expected TurnEnded(.error) on persistence failure, got \(String(describing: t3Events.last))")
        }

        await agent.close()
    }
}
