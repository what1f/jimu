import Darwin
import Dispatch
import Foundation

/// Reusable workspace toolset (`read`, `write`, `edit`, `find`, and `bash` on macOS) with optional `PikraToolApproval`.
public struct PikraWorkspaceTools: Sendable {
    public enum ToolKind: Sendable, Hashable {
        case readFile
        case writeFile
        case editFile
        case shell
        case findFiles
    }

    public let workspace: URL
    public let approval: PikraToolApproval?
    public let pluginID: String
    public let environment: (@Sendable () async -> [String: String])?
    public let shellOutputLimit: Int

    private static let maxReadFileBytes = 16 * 1024 * 1024
    private static let maxShellCommandBytes = 65_536
    private static let defaultShellTimeoutSeconds: Double = 120
    private static let maxShellTimeoutSeconds: Double = 86_400
    private static let defaultFindLimit = 200
    private static let maxFindLimit = 1_000
    private static let skippedFindDirs: Set<String> = [".git", ".build", "node_modules", "target"]

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
    }

    /// Builds the requested workspace tools, wrapping each with `approval` when configured.
    public func tools(
        _ kinds: [ToolKind] = [.readFile, .shell, .editFile, .writeFile]
    ) -> [PikraTool] {
        kinds.compactMap { kind in
            switch kind {
            case .readFile:
                return wrap(makeReadTool(), effect: .readOnly)
            case .writeFile:
                return wrap(makeWriteTool(), effect: .write)
            case .editFile:
                return wrap(makeEditTool(), effect: .write)
            case .findFiles:
                return wrap(makeFindTool(), effect: .readOnly)
            case .shell:
                #if os(macOS)
                return wrap(makeShellTool(), effect: .dangerous)
                #else
                return nil
                #endif
            }
        }
    }

    private func wrap(_ tool: PikraTool, effect: PikraToolEffect) -> PikraTool {
        guard let approval else { return tool }
        return approval.wrap(tool: tool, pluginID: pluginID, effect: effect)
    }

    // MARK: - Path & File Helpers

    private func resolvePath(_ rawPath: String) throws -> URL {
        guard !rawPath.isEmpty, !rawPath.utf8.contains(0) else {
            throw PikraError.invalidArgument("path must be a non-empty string without NUL bytes")
        }
        if rawPath == "~" || rawPath.hasPrefix("~/") {
            let expanded = (rawPath as NSString).expandingTildeInPath
            return URL(fileURLWithPath: expanded).standardizedFileURL
        }
        if rawPath.hasPrefix("/") {
            return URL(fileURLWithPath: rawPath).standardizedFileURL
        }
        return workspace.appendingPathComponent(rawPath).standardizedFileURL
    }

    private static func regularFileSize(at url: URL) throws -> Int {
        var st = Darwin.stat()
        guard stat(url.path, &st) == 0 else {
            let code = errno
            throw PikraError.io("Cannot access \(url.path): \(String(cString: strerror(code)))")
        }
        guard (st.st_mode & S_IFMT) == S_IFREG else {
            throw PikraError.invalidArgument("Path is not a regular file: \(url.path)")
        }
        return Int(st.st_size)
    }

    private static func atomicWrite(_ data: Data, to url: URL) throws {
        let fm = FileManager.default
        let parent = url.deletingLastPathComponent()
        try fm.createDirectory(at: parent, withIntermediateDirectories: true)

        var existingStat = Darwin.stat()
        let existed = stat(url.path, &existingStat) == 0
        let mode: mode_t = existed ? (existingStat.st_mode & 0o777) : 0o600

        let tmpURL = parent.appendingPathComponent(".pikra-tmp-\(UUID().uuidString)")
        let fd = Darwin.open(tmpURL.path, O_WRONLY | O_CREAT | O_EXCL, mode)
        guard fd >= 0 else {
            let code = errno
            throw PikraError.io("Failed to create temp file for \(url.path): \(String(cString: strerror(code)))")
        }

        var closed = false
        func cleanup() {
            if !closed {
                Darwin.close(fd)
                closed = true
            }
            _ = Darwin.unlink(tmpURL.path)
        }

        do {
            try data.withUnsafeBytes { rawBuf in
                guard let base = rawBuf.baseAddress else { return }
                var written = 0
                while written < rawBuf.count {
                    let n = Darwin.write(fd, base.advanced(by: written), rawBuf.count - written)
                    if n < 0 {
                        if errno == EINTR { continue }
                        let code = errno
                        throw PikraError.io("Write failed for \(url.path): \(String(cString: strerror(code)))")
                    }
                    written += n
                }
            }
            _ = Darwin.fchmod(fd, mode)
            if Darwin.close(fd) != 0 {
                closed = true
                let code = errno
                throw PikraError.io("Close failed for \(url.path): \(String(cString: strerror(code)))")
            }
            closed = true

            if Darwin.rename(tmpURL.path, url.path) != 0 {
                let code = errno
                throw PikraError.io("Atomic rename failed for \(url.path): \(String(cString: strerror(code)))")
            }
        } catch {
            cleanup()
            throw error
        }
    }

    private static func detectImageMimeType(_ data: Data) -> String? {
        let bytes = [UInt8](data.prefix(12))
        if bytes.count >= 8,
           bytes[0] == 0x89, bytes[1] == 0x50, bytes[2] == 0x4E, bytes[3] == 0x47,
           bytes[4] == 0x0D, bytes[5] == 0x0A, bytes[6] == 0x1A, bytes[7] == 0x0A
        {
            return "image/png"
        }
        if bytes.count >= 3, bytes[0] == 0xFF, bytes[1] == 0xD8, bytes[2] == 0xFF {
            return "image/jpeg"
        }
        if bytes.count >= 6,
           bytes[0] == 0x47, bytes[1] == 0x49, bytes[2] == 0x46, bytes[3] == 0x38,
           (bytes[4] == 0x37 || bytes[4] == 0x39), bytes[5] == 0x61
        {
            return "image/gif"
        }
        if bytes.count >= 12,
           bytes[0] == 0x52, bytes[1] == 0x49, bytes[2] == 0x46, bytes[3] == 0x46,
           bytes[8] == 0x57, bytes[9] == 0x45, bytes[10] == 0x42, bytes[11] == 0x50
        {
            return "image/webp"
        }
        return nil
    }

    // MARK: - read

    private func makeReadTool() -> PikraTool {
        PikraTool(
            name: "read",
            description: "Read a UTF-8 text file (with optional 1-based line offset and line limit) or an image file (PNG, JPEG, GIF, WebP).",
            parameters: [
                "type": "object",
                "properties": [
                    "path": [
                        "type": "string",
                        "description": "File path relative to the workspace or absolute.",
                    ],
                    "offset": [
                        "type": "integer",
                        "description": "Optional 1-based starting line number.",
                    ],
                    "limit": [
                        "type": "integer",
                        "description": "Optional maximum number of lines to read.",
                    ],
                ],
                "required": ["path"],
            ]
        ) { _, args in
            guard let rawPath = args["path"]?.stringValue, !rawPath.isEmpty else {
                return .error("Missing required string parameter: path")
            }

            let url: URL
            do {
                url = try self.resolvePath(rawPath)
                let size = try Self.regularFileSize(at: url)
                if size > Self.maxReadFileBytes {
                    return .error("File exceeds 16 MiB limit (\(size) bytes): \(url.path)")
                }
            } catch {
                return .error(error.localizedDescription)
            }

            let data: Data
            do {
                data = try Data(contentsOf: url)
            } catch {
                return .error("Failed to read \(url.path): \(error.localizedDescription)")
            }

            if let mimeType = Self.detectImageMimeType(data) {
                return PikraToolOutput(
                    content: [.image(data: data.base64EncodedString(), mimeType: mimeType)],
                    isError: false
                )
            }

            guard let text = String(data: data, encoding: .utf8) else {
                return .error("File is not valid UTF-8 text or supported image: \(url.path)")
            }

            let offset = args["offset"]?.intValue
            let limit = args["limit"]?.intValue
            if let offset, offset < 1 {
                return .error("offset must be >= 1")
            }
            if let limit, limit < 1 {
                return .error("limit must be >= 1")
            }

            guard offset != nil || limit != nil else {
                return .text(text)
            }

            let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
            let startIndex = (offset ?? 1) - 1
            guard startIndex < lines.count else {
                return .text("")
            }
            let endIndex: Int
            if let limit {
                endIndex = min(lines.count, startIndex + limit)
            } else {
                endIndex = lines.count
            }
            return .text(lines[startIndex..<endIndex].joined(separator: "\n"))
        }
    }

    // MARK: - write

    private func makeWriteTool() -> PikraTool {
        PikraTool(
            name: "write",
            description: "Write UTF-8 text content to a file atomically, creating parent directories if needed.",
            parameters: [
                "type": "object",
                "properties": [
                    "path": [
                        "type": "string",
                        "description": "File path relative to the workspace or absolute.",
                    ],
                    "content": [
                        "type": "string",
                        "description": "Full text content to write.",
                    ],
                ],
                "required": ["path", "content"],
            ]
        ) { _, args in
            guard let rawPath = args["path"]?.stringValue, !rawPath.isEmpty else {
                return .error("Missing required string parameter: path")
            }
            guard let content = args["content"]?.stringValue else {
                return .error("Missing required string parameter: content")
            }

            do {
                let url = try self.resolvePath(rawPath)
                let data = Data(content.utf8)
                try Self.atomicWrite(data, to: url)
                return .text("Wrote \(data.count) bytes to \(url.path)")
            } catch {
                return .error(error.localizedDescription)
            }
        }
    }

    // MARK: - edit

    private func makeEditTool() -> PikraTool {
        PikraTool(
            name: "edit",
            description: "Replace a single unique occurrence of old_text with new_text in a UTF-8 file atomically.",
            parameters: [
                "type": "object",
                "properties": [
                    "path": [
                        "type": "string",
                        "description": "File path relative to the workspace or absolute.",
                    ],
                    "old_text": [
                        "type": "string",
                        "description": "Exact existing text to replace (must occur exactly once).",
                    ],
                    "new_text": [
                        "type": "string",
                        "description": "Replacement text.",
                    ],
                ],
                "required": ["path", "old_text", "new_text"],
            ]
        ) { _, args in
            guard let rawPath = args["path"]?.stringValue, !rawPath.isEmpty else {
                return .error("Missing required string parameter: path")
            }
            guard let oldText = args["old_text"]?.stringValue ?? args["oldText"]?.stringValue,
                  !oldText.isEmpty
            else {
                return .error("Missing or empty required string parameter: old_text")
            }
            guard let newText = args["new_text"]?.stringValue ?? args["newText"]?.stringValue else {
                return .error("Missing required string parameter: new_text")
            }

            do {
                let url = try self.resolvePath(rawPath)
                let size = try Self.regularFileSize(at: url)
                if size > Self.maxReadFileBytes {
                    return .error("File exceeds 16 MiB limit (\(size) bytes): \(url.path)")
                }
                let data = try Data(contentsOf: url)
                guard let content = String(data: data, encoding: .utf8) else {
                    return .error("File is not valid UTF-8 text: \(url.path)")
                }

                guard let firstRange = content.range(of: oldText) else {
                    return .error("old_text not found in \(url.path)")
                }
                if content.range(of: oldText, range: firstRange.upperBound..<content.endIndex) != nil {
                    return .error("old_text matched multiple times in \(url.path); provide more context for a unique match")
                }

                var updated = content
                updated.replaceSubrange(firstRange, with: newText)
                try Self.atomicWrite(Data(updated.utf8), to: url)
                return .text("Edited \(url.path)")
            } catch {
                return .error(error.localizedDescription)
            }
        }
    }

    // MARK: - find

    private func makeFindTool() -> PikraTool {
        PikraTool(
            name: "find",
            description: "Find files under the workspace matching a glob or substring pattern.",
            parameters: [
                "type": "object",
                "properties": [
                    "pattern": [
                        "type": "string",
                        "description": "Glob pattern (e.g. '*.swift', '**/*.rs') or case-insensitive substring.",
                    ],
                    "limit": [
                        "type": "integer",
                        "description": "Optional maximum number of matching paths to return.",
                    ],
                ],
                "required": ["pattern"],
            ]
        ) { _, args in
            guard let pattern = args["pattern"]?.stringValue, !pattern.isEmpty else {
                return .error("Missing required string parameter: pattern")
            }
            let requestedLimit = args["limit"]?.intValue ?? Self.defaultFindLimit
            let maxResults = min(max(1, requestedLimit), Self.maxFindLimit)

            let fm = FileManager.default
            guard let enumerator = fm.enumerator(
                at: self.workspace,
                includingPropertiesForKeys: [.isDirectoryKey],
                options: []
            ) else {
                return .error("Failed to enumerate workspace: \(self.workspace.path)")
            }

            let rootPrefix = self.workspace.path.hasSuffix("/")
                ? self.workspace.path
                : self.workspace.path + "/"
            let isGlob = pattern.contains("*") || pattern.contains("?") || pattern.contains("[")
            let normalizedGlob = pattern.hasPrefix("**/") ? String(pattern.dropFirst(3)) : pattern
            let loweredPattern = pattern.lowercased()

            var matches: [String] = []
            while let item = enumerator.nextObject() as? URL {
                let name = item.lastPathComponent
                let isDir = (try? item.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) ?? false
                if isDir {
                    if Self.skippedFindDirs.contains(name) {
                        enumerator.skipDescendants()
                    }
                    continue
                }

                let fullPath = item.standardizedFileURL.path
                let relPath = fullPath.hasPrefix(rootPrefix)
                    ? String(fullPath.dropFirst(rootPrefix.count))
                    : name

                let matched: Bool
                if isGlob {
                    matched = fnmatch(pattern, relPath, FNM_CASEFOLD) == 0 ||
                        fnmatch(normalizedGlob, relPath, FNM_CASEFOLD) == 0 ||
                        fnmatch(normalizedGlob, name, FNM_CASEFOLD) == 0
                } else {
                    matched = relPath.lowercased().contains(loweredPattern)
                }

                if matched {
                    matches.append(relPath)
                    if matches.count >= maxResults {
                        break
                    }
                }
            }

            matches.sort()
            return .text(matches.joined(separator: "\n"))
        }
    }

    // MARK: - bash (macOS only)

    #if os(macOS)
    private func makeShellTool() -> PikraTool {
        PikraTool(
            name: "bash",
            description: "Execute a shell command in the workspace directory and return exit code and combined output.",
            parameters: [
                "type": "object",
                "properties": [
                    "command": [
                        "type": "string",
                        "description": "Shell command to run (1..65536 UTF-8 bytes, no NUL).",
                    ],
                    "timeout": [
                        "type": "number",
                        "description": "Optional timeout in seconds (0 < timeout <= 86400, default 120).",
                    ],
                ],
                "required": ["command"],
            ]
        ) { _, args in
            guard let command = args["command"]?.stringValue else {
                return .error("Missing required string parameter: command")
            }
            let byteCount = command.utf8.count
            guard byteCount >= 1 && byteCount <= Self.maxShellCommandBytes else {
                return .error("command length must be between 1 and \(Self.maxShellCommandBytes) UTF-8 bytes")
            }
            guard !command.utf8.contains(0) else {
                return .error("command must not contain NUL bytes")
            }

            let timeout = args["timeout"]?.doubleValue ?? Self.defaultShellTimeoutSeconds
            guard timeout.isFinite && timeout > 0 && timeout <= Self.maxShellTimeoutSeconds else {
                return .error("timeout must be > 0 and <= \(Int(Self.maxShellTimeoutSeconds)) seconds")
            }

            var env = ProcessInfo.processInfo.environment
            if let envProvider = self.environment {
                let extra = await envProvider()
                for (k, v) in extra {
                    env[k] = v
                }
            }

            let result = try await Self.runShellProcess(
                command: command,
                cwd: self.workspace,
                environment: env,
                timeoutSeconds: timeout,
                outputLimit: self.shellOutputLimit
            )

            let payload: JSONValue = [
                "exit_code": .number(Double(result.exitCode)),
                "output": .string(result.output),
                "output_truncated": .bool(result.outputTruncated),
            ]
            let encoded = (try? encodeJSONString(payload)) ?? "{\"exit_code\":\(result.exitCode),\"output\":\"\",\"output_truncated\":false}"
            return PikraToolOutput(
                content: [.text(encoded)],
                details: .null,
                structuredContent: payload,
                isError: result.exitCode != 0,
                terminate: false
            )
        }
    }

    private struct ShellResult: Sendable {
        let exitCode: Int32
        let output: String
        let outputTruncated: Bool
    }

    private final class ShellProcessGuard: @unchecked Sendable {
        private let lock = NSLock()
        private var pid: pid_t = 0
        private var exited = false
        private var cancelled = false
        private var timedOut = false
        private var killTimer: DispatchSourceTimer?

        func attach(pid: pid_t) -> Bool {
            lock.withLock {
                self.pid = pid
                return !cancelled
            }
        }

        func markExited() {
            let timer = lock.withLock { () -> DispatchSourceTimer? in
                exited = true
                let t = killTimer
                killTimer = nil
                return t
            }
            timer?.cancel()
        }

        var wasCancelled: Bool {
            lock.withLock { cancelled }
        }

        var didTimeOut: Bool {
            lock.withLock { timedOut }
        }

        func cancel(becauseTimedOut: Bool) {
            let targetPid: pid_t = lock.withLock {
                if becauseTimedOut {
                    timedOut = true
                } else {
                    cancelled = true
                }
                guard !exited, pid > 0 else { return 0 }
                return pid
            }
            guard targetPid > 0 else { return }
            _ = Darwin.killpg(targetPid, SIGTERM)

            let graceTimer = DispatchSource.makeTimerSource(queue: DispatchQueue.global(qos: .utility))
            graceTimer.schedule(deadline: .now() + .milliseconds(200))
            graceTimer.setEventHandler { [weak self] in
                guard let self else { return }
                let stillAlive: pid_t = self.lock.withLock {
                    self.killTimer = nil
                    return self.exited ? 0 : self.pid
                }
                if stillAlive > 0 {
                    _ = Darwin.killpg(stillAlive, SIGKILL)
                }
            }
            let shouldResume = lock.withLock { () -> Bool in
                guard !exited else { return false }
                killTimer?.cancel()
                killTimer = graceTimer
                return true
            }
            if shouldResume {
                graceTimer.resume()
            } else {
                graceTimer.cancel()
            }
        }
    }

    private static func runShellProcess(
        command: String,
        cwd: URL,
        environment: [String: String],
        timeoutSeconds: Double,
        outputLimit: Int
    ) async throws -> ShellResult {
        try Task.checkCancellation()

        let shellPath = FileManager.default.isExecutableFile(atPath: "/bin/zsh") ? "/bin/zsh" : "/bin/sh"
        var pipeFds: [Int32] = [-1, -1]
        guard Darwin.pipe(&pipeFds) == 0 else {
            throw PikraError.io("pipe() failed: \(String(cString: strerror(errno)))")
        }
        let readFd = pipeFds[0]
        let writeFd = pipeFds[1]

        let devNullFd = Darwin.open("/dev/null", O_RDONLY)
        guard devNullFd >= 0 else {
            let code = errno
            Darwin.close(readFd)
            Darwin.close(writeFd)
            throw PikraError.io("open(/dev/null) failed: \(String(cString: strerror(code)))")
        }

        var fileActions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&fileActions)
        defer { posix_spawn_file_actions_destroy(&fileActions) }
        posix_spawn_file_actions_adddup2(&fileActions, devNullFd, STDIN_FILENO)
        posix_spawn_file_actions_adddup2(&fileActions, writeFd, STDOUT_FILENO)
        posix_spawn_file_actions_adddup2(&fileActions, writeFd, STDERR_FILENO)
        posix_spawn_file_actions_addclose(&fileActions, devNullFd)
        posix_spawn_file_actions_addclose(&fileActions, readFd)
        posix_spawn_file_actions_addclose(&fileActions, writeFd)
        posix_spawn_file_actions_addchdir_np(&fileActions, cwd.path)

        var spawnAttr: posix_spawnattr_t?
        posix_spawnattr_init(&spawnAttr)
        defer { posix_spawnattr_destroy(&spawnAttr) }
        let spawnFlags = Int16(POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_CLOEXEC_DEFAULT)
        posix_spawnattr_setflags(&spawnAttr, spawnFlags)
        posix_spawnattr_setpgroup(&spawnAttr, 0)

        let argvStrings = [shellPath, "-c", command]
        var argv: [UnsafeMutablePointer<CChar>?] = argvStrings.map { strdup($0) }
        argv.append(nil)
        defer { for ptr in argv { free(ptr) } }

        let envStrings = environment.map { "\($0.key)=\($0.value)" }
        var envp: [UnsafeMutablePointer<CChar>?] = envStrings.map { strdup($0) }
        envp.append(nil)
        defer { for ptr in envp { free(ptr) } }

        var spawnedPid: pid_t = 0
        let spawnStatus = posix_spawn(
            &spawnedPid,
            shellPath,
            &fileActions,
            &spawnAttr,
            &argv,
            &envp
        )
        Darwin.close(devNullFd)
        Darwin.close(writeFd)

        guard spawnStatus == 0 else {
            Darwin.close(readFd)
            throw PikraError.io("posix_spawn(\(shellPath)) failed: \(String(cString: strerror(spawnStatus)))")
        }

        let childPid = spawnedPid
        let guardBox = ShellProcessGuard()
        if !guardBox.attach(pid: childPid) {
            guardBox.cancel(becauseTimedOut: false)
        }

        let timeoutTimer = DispatchSource.makeTimerSource(queue: DispatchQueue.global(qos: .utility))
        let timeoutNs = Int64(min(timeoutSeconds * 1_000_000_000.0, Double(Int64.max / 2)))
        timeoutTimer.schedule(deadline: .now() + .nanoseconds(Int(timeoutNs)))
        timeoutTimer.setEventHandler {
            guardBox.cancel(becauseTimedOut: true)
        }
        timeoutTimer.resume()

        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                DispatchQueue.global(qos: .userInitiated).async {
                    defer {
                        timeoutTimer.cancel()
                        Darwin.close(readFd)
                    }

                    // Drain combined stdout/stderr up to `outputLimit + 4` bytes (extra 4 bytes to preserve UTF-8 boundary check).
                    let capBytes = outputLimit + 4
                    var captured = Data()
                    var truncated = false
                    var buffer = [UInt8](repeating: 0, count: 16 * 1024)

                    while true {
                        let n = Darwin.read(readFd, &buffer, buffer.count)
                        if n > 0 {
                            if captured.count < capBytes {
                                let room = capBytes - captured.count
                                let toTake = min(room, n)
                                captured.append(contentsOf: buffer[0..<toTake])
                                if captured.count > outputLimit {
                                    truncated = true
                                }
                                if toTake < n {
                                    truncated = true
                                }
                            } else {
                                truncated = true
                            }
                        } else if n == 0 {
                            break
                        } else if errno == EINTR {
                            continue
                        } else {
                            break
                        }
                    }

                    var rawStatus: Int32 = 0
                    while true {
                        let waited = Darwin.waitpid(childPid, &rawStatus, 0)
                        if waited == childPid { break }
                        if waited < 0 && errno == EINTR { continue }
                        break
                    }
                    guardBox.markExited()

                    if guardBox.wasCancelled {
                        continuation.resume(throwing: CancellationError())
                        return
                    }

                    let signalCode = rawStatus & 0x7F
                    let exitCode: Int32
                    if guardBox.didTimeOut {
                        exitCode = 124
                    } else if signalCode == 0 {
                        exitCode = (rawStatus >> 8) & 0xFF
                    } else {
                        exitCode = 128 + signalCode
                    }

                    let output = Self.utf8SafePrefix(captured, limit: outputLimit)
                    continuation.resume(
                        returning: ShellResult(
                            exitCode: exitCode,
                            output: output,
                            outputTruncated: truncated
                        )
                    )
                }
            }
        } onCancel: {
            guardBox.cancel(becauseTimedOut: false)
        }
    }

    private static func utf8SafePrefix(_ data: Data, limit: Int) -> String {
        guard limit > 0 else { return "" }
        guard data.count > limit else {
            return String(decoding: data, as: UTF8.self)
        }
        var leadIdx = limit - 1
        var steps = 0
        while leadIdx > 0 && steps < 3 && (data[leadIdx] & 0xC0) == 0x80 {
            leadIdx -= 1
            steps += 1
        }
        let lead = data[leadIdx]
        let seqLen: Int
        if (lead & 0x80) == 0 {
            seqLen = 1
        } else if (lead & 0xE0) == 0xC0 {
            seqLen = 2
        } else if (lead & 0xF0) == 0xE0 {
            seqLen = 3
        } else if (lead & 0xF8) == 0xF0 {
            seqLen = 4
        } else {
            seqLen = 1
        }
        let cut = (leadIdx + seqLen <= limit) ? limit : leadIdx
        return String(decoding: data.prefix(cut), as: UTF8.self)
    }
    #endif
}
