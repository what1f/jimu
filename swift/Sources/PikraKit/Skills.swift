import CPikra
import Darwin
import Dispatch
import Foundation

/// Pure-FFI skill discovery returning parsed descriptors, content revisions, and XML prompt block.
public struct PikraSkillDiscovery: Sendable {
    public struct Entry: Sendable, Equatable {
        public let name: String
        public let description: String
        public let location: URL
        public let disableModelInvocation: Bool
        public let revision: String

        public init(
            name: String,
            description: String,
            location: URL,
            disableModelInvocation: Bool,
            revision: String
        ) {
            self.name = name
            self.description = description
            self.location = location
            self.disableModelInvocation = disableModelInvocation
            self.revision = revision
        }
    }

    public struct Catalog: Sendable, Equatable {
        public let skills: [Entry]
        public let prompt: String

        public init(skills: [Entry], prompt: String) {
            self.skills = skills
            self.prompt = prompt
        }
    }

    private struct WireEntry: Decodable {
        let name: String
        let description: String
        let location: String
        let disableModelInvocation: Bool
        let revision: String

        enum CodingKeys: String, CodingKey {
            case name
            case description
            case location
            case disableModelInvocation = "disable_model_invocation"
            case revision
        }
    }

    private struct WireResponse: Decodable {
        let skills: [WireEntry]
        let prompt: String
    }

    public static func discover(roots: [URL]) throws -> Catalog {
        let rootsJSON = try encodeJSONString(roots.map(\.path))
        var errorPtr: UnsafeMutablePointer<CChar>?
        let rawPtr = rootsJSON.withCString { cstr in
            pikra_discover_skills(cstr, &errorPtr)
        }
        guard let jsonStr = takeOwnedCString(rawPtr) else {
            let message = takeOwnedCString(errorPtr) ?? "Failed to discover skills"
            throw PikraError.io(message)
        }
        guard let data = jsonStr.data(using: .utf8) else {
            throw PikraError.internalError("Invalid UTF-8 returned by pikra_discover_skills")
        }
        let decoded = try JSONDecoder().decode(WireResponse.self, from: data)
        let entries = decoded.skills.map { wire in
            Entry(
                name: wire.name,
                description: wire.description,
                location: URL(fileURLWithPath: wire.location),
                disableModelInvocation: wire.disableModelInvocation,
                revision: wire.revision
            )
        }
        return Catalog(skills: entries, prompt: decoded.prompt)
    }
}

/// Event-driven skill directory watcher backed by `DispatchSource.makeFileSystemObjectSource` (`O_EVTONLY`, zero polling).
public actor PikraSkillCatalog {
    public let roots: [URL]
    public nonisolated let updates: AsyncStream<Void>

    private let watcher: WatcherBox

    public init(roots: [URL]) {
        self.roots = roots
        var capturedContinuation: AsyncStream<Void>.Continuation?
        self.updates = AsyncStream<Void>(bufferingPolicy: .bufferingNewest(1)) { continuation in
            capturedContinuation = continuation
        }
        self.watcher = WatcherBox(roots: roots, continuation: capturedContinuation!)
        self.watcher.start()
    }

    deinit {
        watcher.close()
    }

    /// Runs `PikraSkillDiscovery.discover(roots:)` and refreshes watched skill subdirectories.
    public func discover() throws -> PikraSkillDiscovery.Catalog {
        watcher.resync()
        return try PikraSkillDiscovery.discover(roots: roots)
    }

    /// Cancels all file system event sources and finishes the `updates` stream.
    public func close() {
        watcher.close()
    }

    private final class WatcherBox: @unchecked Sendable {
        private let roots: [URL]
        private let continuation: AsyncStream<Void>.Continuation
        private let queue = DispatchQueue(label: "pikra.skills.watcher")
        private let lock = NSLock()
        private var sources: [String: DispatchSourceFileSystemObject] = [:]
        private var closed = false

        init(roots: [URL], continuation: AsyncStream<Void>.Continuation) {
            self.roots = roots
            self.continuation = continuation
        }

        func start() {
            lock.withLock {
                guard !closed else { return }
                syncSourcesLocked()
            }
        }

        func resync() {
            lock.withLock {
                guard !closed else { return }
                syncSourcesLocked()
            }
        }

        func close() {
            let toCancel: [DispatchSourceFileSystemObject] = lock.withLock {
                guard !closed else { return [] }
                closed = true
                let list = Array(sources.values)
                sources.removeAll()
                return list
            }
            for source in toCancel {
                source.cancel()
            }
            continuation.finish()
        }

        private func handleEvent(for path: String, flags: DispatchSource.FileSystemEvent) {
            let shouldYield = lock.withLock { () -> Bool in
                guard !closed else { return false }
                if flags.contains(.delete) || flags.contains(.rename) {
                    if let stale = sources.removeValue(forKey: path) {
                        stale.cancel()
                    }
                }
                syncSourcesLocked()
                return true
            }
            if shouldYield {
                continuation.yield(())
            }
        }

        private func syncSourcesLocked() {
            let desiredPaths = collectWatchTargets()

            // Remove watchers for paths that no longer exist.
            for (path, source) in sources where !desiredPaths.contains(path) {
                sources.removeValue(forKey: path)
                source.cancel()
            }

            // Add watchers for newly discovered directories & skill files.
            for path in desiredPaths where sources[path] == nil {
                guard let source = makeSource(for: path) else { continue }
                sources[path] = source
                source.resume()
            }
        }

        private func collectWatchTargets() -> Set<String> {
            let fm = FileManager.default
            var targets: Set<String> = []

            func scanDirectory(_ dirURL: URL, includeRootMarkdown: Bool) {
                let dirPath = dirURL.standardizedFileURL.path
                targets.insert(dirPath)

                guard let children = try? fm.contentsOfDirectory(atPath: dirPath) else {
                    return
                }

                let skillMDURL = dirURL.appendingPathComponent("SKILL.md")
                let skillMDPath = skillMDURL.standardizedFileURL.path
                var skillIsDir: ObjCBool = false
                if !includeRootMarkdown,
                   fm.fileExists(atPath: skillMDPath, isDirectory: &skillIsDir),
                   !skillIsDir.boolValue
                {
                    targets.insert(skillMDPath)
                    return
                }

                for child in children {
                    if child.hasPrefix(".") || child == "node_modules" {
                        continue
                    }
                    let childURL = dirURL.appendingPathComponent(child)
                    let childPath = childURL.standardizedFileURL.path
                    var childIsDir: ObjCBool = false
                    guard fm.fileExists(atPath: childPath, isDirectory: &childIsDir) else { continue }

                    if childIsDir.boolValue {
                        scanDirectory(childURL, includeRootMarkdown: false)
                    } else if child == "SKILL.md" || (includeRootMarkdown && child.hasSuffix(".md")) {
                        targets.insert(childPath)
                    }
                }
            }

            for root in roots {
                let rootPath = root.standardizedFileURL.path
                var isDir: ObjCBool = false
                guard fm.fileExists(atPath: rootPath, isDirectory: &isDir) else { continue }
                if isDir.boolValue {
                    scanDirectory(root, includeRootMarkdown: true)
                } else if rootPath.hasSuffix(".md") {
                    targets.insert(rootPath)
                }
            }
            return targets
        }

        private func makeSource(for path: String) -> DispatchSourceFileSystemObject? {
            let fd = Darwin.open(path, O_EVTONLY)
            guard fd >= 0 else { return nil }

            let source = DispatchSource.makeFileSystemObjectSource(
                fileDescriptor: fd,
                eventMask: [.write, .rename, .delete, .attrib],
                queue: queue
            )
            source.setEventHandler { [weak self, weak source] in
                guard let self else { return }
                let flags = source?.data ?? []
                self.handleEvent(for: path, flags: flags)
            }
            source.setCancelHandler {
                Darwin.close(fd)
            }
            return source
        }
    }
}
