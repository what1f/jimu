import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// Outbound HTTP request constructed by Rust `pikra-ai` and dispatched by Swift.
public struct PikraHTTPRequest: Sendable, Equatable {
    public let method: String
    public let url: URL
    public let headers: [(String, String)]
    public let body: Data
    public let timeoutInterval: TimeInterval?

    public init(
        method: String,
        url: URL,
        headers: [(String, String)] = [],
        body: Data = Data(),
        timeoutInterval: TimeInterval? = nil
    ) {
        self.method = method
        self.url = url
        self.headers = headers
        self.body = body
        self.timeoutInterval = timeoutInterval
    }

    public static func == (lhs: PikraHTTPRequest, rhs: PikraHTTPRequest) -> Bool {
        lhs.method == rhs.method &&
            lhs.url == rhs.url &&
            lhs.body == rhs.body &&
            lhs.timeoutInterval == rhs.timeoutInterval &&
            lhs.headers.count == rhs.headers.count &&
            zip(lhs.headers, rhs.headers).allSatisfy { $0.0 == $1.0 && $0.1 == $1.1 }
    }

    public func toURLRequest() -> URLRequest {
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.cachePolicy = .reloadIgnoringLocalCacheData
        if let timeoutInterval {
            req.timeoutInterval = timeoutInterval
        }
        for (name, value) in headers {
            req.addValue(value, forHTTPHeaderField: name)
        }
        if !body.isEmpty {
            req.httpBody = body
        }
        return req
    }
}

/// Streaming HTTP response returned by a `PikraHTTPClient`.
public struct PikraHTTPResponse: Sendable {
    public let statusCode: Int
    public let headers: [(String, String)]
    public let body: AsyncThrowingStream<Data, any Error>
    private let cancelHandler: @Sendable () -> Void

    public init(
        statusCode: Int,
        headers: [(String, String)] = [],
        body: AsyncThrowingStream<Data, any Error>,
        cancel: @escaping @Sendable () -> Void = {}
    ) {
        self.statusCode = statusCode
        self.headers = headers
        self.body = body
        self.cancelHandler = cancel
    }

    public func cancel() {
        cancelHandler()
    }
}

/// Host HTTP transport protocol plugged into `PikraAgent`.
public protocol PikraHTTPClient: Sendable {
    func send(_ request: PikraHTTPRequest) async throws -> PikraHTTPResponse
}

/// Default `URLSession` streaming transport backed by an ephemeral, non-persistent session.
///
/// API keys, headers, cookies, and responses NEVER touch disk caches.
public struct PikraURLSessionHTTPClient: PikraHTTPClient {
    private let session: URLSession
    public let beforeRequest: (@Sendable (Data) async throws -> Void)?

    /// Ephemeral `URLSession` with all disk caches, credential stores, and cookie stores disabled.
    private static let ephemeralSession: URLSession = {
        let config = URLSessionConfiguration.ephemeral
        config.urlCache = nil
        config.urlCredentialStorage = nil
        config.httpCookieStorage = nil
        config.httpShouldSetCookies = false
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        return URLSession(configuration: config)
    }()

    /// Verifies that the underlying session does not retain persistent disk HTTP state.
    public var permitsPersistentHTTPState: Bool {
        let cfg = session.configuration
        return cfg.urlCache != nil ||
            cfg.urlCredentialStorage != nil ||
            cfg.httpCookieStorage != nil ||
            cfg.httpShouldSetCookies
    }

    public init(beforeRequest: (@Sendable (Data) async throws -> Void)? = nil) {
        self.session = Self.ephemeralSession
        self.beforeRequest = beforeRequest
    }

    public init(
        session: URLSession,
        beforeRequest: (@Sendable (Data) async throws -> Void)? = nil
    ) {
        self.session = session
        self.beforeRequest = beforeRequest
    }

    public func send(_ request: PikraHTTPRequest) async throws -> PikraHTTPResponse {
        try Task.checkCancellation()
        if let beforeRequest {
            try await beforeRequest(request.body)
        }
        try Task.checkCancellation()

        let urlRequest = request.toURLRequest()
        let (bytes, response) = try await session.bytes(
            for: urlRequest,
            delegate: CredentialRedirectPolicy()
        )
        if Task.isCancelled {
            bytes.task.cancel()
            throw CancellationError()
        }
        guard let httpResponse = response as? HTTPURLResponse else {
            bytes.task.cancel()
            throw PikraError.transport("Invalid non-HTTP URLResponse")
        }

        let reader = AsyncBytesChunkReader(bytes)
        let body = AsyncThrowingStream<Data, any Error> {
            try await reader.nextChunk()
        }
        let headers: [(String, String)] = httpResponse.allHeaderFields.map { key, value in
            (String(describing: key).lowercased(), String(describing: value))
        }
        return PikraHTTPResponse(
            statusCode: httpResponse.statusCode,
            headers: headers,
            body: body,
            cancel: { reader.cancel() }
        )
    }
}

private final class AsyncBytesChunkReader: @unchecked Sendable {
    private let lock = NSLock()
    private let task: URLSessionTask
    private var iterator: URLSession.AsyncBytes.Iterator?
    private var ended = false
    private var cancelled = false

    init(_ bytes: URLSession.AsyncBytes) {
        self.task = bytes.task
        self.iterator = bytes.makeAsyncIterator()
    }

    deinit {
        task.cancel()
    }

    func cancel() {
        lock.withLock {
            cancelled = true
            iterator = nil
        }
        task.cancel()
    }

    func nextChunk() async throws -> Data? {
        try await withTaskCancellationHandler {
            try Task.checkCancellation()
            let currentIterator = try lock.withLock { () throws -> URLSession.AsyncBytes.Iterator? in
                guard !cancelled else { throw CancellationError() }
                if ended { return nil }
                guard let iter = iterator else {
                    throw PikraError.transport("Concurrent HTTP body reads are not allowed")
                }
                iterator = nil
                return iter
            }
            guard var iter = currentIterator else { return nil }
            defer {
                lock.withLock {
                    if !cancelled && !ended {
                        iterator = iter
                    }
                }
            }
            do {
                var chunk = Data()
                while let byte = try await iter.next() {
                    try Task.checkCancellation()
                    chunk.append(byte)
                    // Flush on LF (SSE line boundary) or 16 KiB chunk boundary for low latency.
                    if byte == 0x0A || chunk.count >= 16 * 1024 {
                        try lock.withLock {
                            if cancelled { throw CancellationError() }
                        }
                        return chunk
                    }
                }
                try Task.checkCancellation()
                try lock.withLock {
                    guard !cancelled else { throw CancellationError() }
                    ended = true
                }
                return chunk.isEmpty ? nil : chunk
            } catch {
                cancel()
                throw error
            }
        } onCancel: {
            self.cancel()
        }
    }
}

/// Prevent forwarding `Authorization` headers or POST request bodies across redirects.
private final class CredentialRedirectPolicy: NSObject, URLSessionTaskDelegate {
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest
    ) async -> URLRequest? {
        guard let original = task.originalRequest,
              original.value(forHTTPHeaderField: "Authorization") == nil,
              original.value(forHTTPHeaderField: "x-api-key") == nil,
              original.value(forHTTPHeaderField: "x-goog-api-key") == nil,
              ["GET", "HEAD"].contains(original.httpMethod ?? "GET")
        else {
            return nil
        }
        return request
    }
}
