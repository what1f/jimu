import CPikra
import Foundation

public enum PikraWalkBoundary: String, Codable, Sendable {
    case projectRoot = "project_root"
    case filesystemRoot = "filesystem_root"
    case currentDirOnly = "current_dir_only"
}

/// Discovers instruction files in order from broadest to most specific.
public enum PikraInstructionDiscovery {
    public struct Configuration: Encodable, Sendable {
        public var cwd: URL?
        public var additionalDirectories: [URL]
        public var walkBoundary: PikraWalkBoundary
        public var projectRootMarkers: [String]
        public var filenames: [String]
        public var includeUserInstructions: Bool
        public var includeAgentsDir: Bool
        public var userDirs: [URL]
        public var homeDirOverride: URL?
        public var extraFiles: [URL]
        /// The shared content budget in bytes. Pass nil for unlimited or zero to disable loading.
        public var maxBytes: Int?
        public var deduplicateIdenticalContent: Bool

        public init(
            cwd: URL? = nil,
            additionalDirectories: [URL] = [],
            walkBoundary: PikraWalkBoundary = .projectRoot,
            projectRootMarkers: [String] = [".git"],
            filenames: [String] = ["AGENTS.override.md", "AGENTS.md"],
            includeUserInstructions: Bool = true,
            includeAgentsDir: Bool = true,
            userDirs: [URL] = [],
            homeDirOverride: URL? = nil,
            extraFiles: [URL] = [],
            maxBytes: Int? = 32 * 1024,
            deduplicateIdenticalContent: Bool = true
        ) {
            self.cwd = cwd
            self.additionalDirectories = additionalDirectories
            self.walkBoundary = walkBoundary
            self.projectRootMarkers = projectRootMarkers
            self.filenames = filenames
            self.includeUserInstructions = includeUserInstructions
            self.includeAgentsDir = includeAgentsDir
            self.userDirs = userDirs
            self.homeDirOverride = homeDirOverride
            self.extraFiles = extraFiles
            self.maxBytes = maxBytes
            self.deduplicateIdenticalContent = deduplicateIdenticalContent
        }

        private enum CodingKeys: String, CodingKey {
            case cwd, filenames
            case additionalDirectories = "additional_directories"
            case walkBoundary = "walk_boundary"
            case projectRootMarkers = "project_root_markers"
            case includeUserInstructions = "include_user_instructions"
            case includeAgentsDir = "include_agents_dir"
            case userDirs = "user_dirs"
            case homeDirOverride = "home_dir_override"
            case extraFiles = "extra_files"
            case maxBytes = "max_bytes"
            case deduplicateIdenticalContent = "deduplicate_identical_content"
        }

        public func encode(to encoder: any Encoder) throws {
            var container = encoder.container(keyedBy: CodingKeys.self)
            try container.encodeIfPresent(cwd?.path, forKey: .cwd)
            try container.encode(additionalDirectories.map(\.path), forKey: .additionalDirectories)
            try container.encode(walkBoundary, forKey: .walkBoundary)
            try container.encode(projectRootMarkers, forKey: .projectRootMarkers)
            try container.encode(filenames, forKey: .filenames)
            try container.encode(includeUserInstructions, forKey: .includeUserInstructions)
            try container.encode(includeAgentsDir, forKey: .includeAgentsDir)
            try container.encode(userDirs.map(\.path), forKey: .userDirs)
            try container.encodeIfPresent(homeDirOverride?.path, forKey: .homeDirOverride)
            try container.encode(extraFiles.map(\.path), forKey: .extraFiles)
            try container.encode(maxBytes, forKey: .maxBytes)
            try container.encode(deduplicateIdenticalContent, forKey: .deduplicateIdenticalContent)
        }
    }

    public struct Entry: Decodable, Sendable, Equatable {
        public let path: URL
        public let content: String
        public let truncated: Bool

        private enum CodingKeys: String, CodingKey {
            case path, content, truncated
        }

        public init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            path = URL(fileURLWithPath: try container.decode(String.self, forKey: .path))
            content = try container.decode(String.self, forKey: .content)
            truncated = try container.decode(Bool.self, forKey: .truncated)
        }
    }

    public struct Catalog: Decodable, Sendable, Equatable {
        public let entries: [Entry]
        public let totalBytes: Int
        public let truncated: Bool
        public let prompt: String?
        public var isEmpty: Bool { entries.isEmpty }

        private enum CodingKeys: String, CodingKey {
            case entries, truncated, prompt
            case totalBytes = "total_bytes"
        }
    }

    public static func discover(config: Configuration) throws -> Catalog {
        let json = try encodeJSONString(config)
        var errorPtr: UnsafeMutablePointer<CChar>?
        let result = json.withCString { pikra_discover_instructions($0, &errorPtr) }
        guard let response = takeOwnedCString(result) else {
            throw PikraError.io(takeOwnedCString(errorPtr) ?? "Failed to discover instructions")
        }
        return try JSONDecoder().decode(Catalog.self, from: Data(response.utf8))
    }
}
