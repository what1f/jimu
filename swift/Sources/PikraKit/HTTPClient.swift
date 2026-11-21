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

    public init?(urlRequest: URLRequest) {
        guard let url = urlRequest.url else { return nil }
        let sortedHeaders = (urlRequest.allHTTPHeaderFields ?? [:])
            .sorted { $0.key.lowercased() < $1.key.lowercased() }
            .map { ($0.key, $0.value) }
        self.init(
            method: urlRequest.httpMethod ?? "GET",
            url: url,
            headers: sortedHeaders,
            body: urlRequest.httpBody ?? Data(),
            timeoutInterval: urlRequest.timeoutInterval
        )
    }

    init(applying urlRequest: URLRequest, fallback: PikraHTTPRequest, originalTimeout: TimeInterval) {
        let sortedHeaders = (urlRequest.allHTTPHeaderFields ?? [:])
            .sorted { $0.key.lowercased() < $1.key.lowercased() }
            .map { ($0.key, $0.value) }
        let effectiveTimeout: TimeInterval? =
            urlRequest.timeoutInterval == originalTimeout
            ? fallback.timeoutInterval
            : urlRequest.timeoutInterval
        self.init(
            method: urlRequest.httpMethod ?? fallback.method,
            url: urlRequest.url ?? fallback.url,
            headers: sortedHeaders,
            body: urlRequest.httpBody ?? Data(),
            timeoutInterval: effectiveTimeout
        )
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

        var urlRequest = request.toURLRequest()
        let manualRedirect = urlRequest.value(forHTTPHeaderField: "x-pikra-no-redirect") != nil
        urlRequest.setValue(nil, forHTTPHeaderField: "x-pikra-no-redirect")
        let (bytes, response) = try await session.bytes(
            for: urlRequest,
            delegate: CredentialRedirectPolicy(manualRedirect: manualRedirect)
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

/// Leave anonymous redirects to the caller. Authenticated requests can redirect within their origin.
private final class CredentialRedirectPolicy: NSObject, URLSessionTaskDelegate {
    private let manualRedirect: Bool
    private static let credentialHeaders = ["Authorization", "x-api-key", "x-goog-api-key", "chatgpt-account-id"]

    init(manualRedirect: Bool) {
        self.manualRedirect = manualRedirect
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest
    ) async -> URLRequest? {
        guard !manualRedirect, let original = task.originalRequest,
              Self.credentialHeaders.contains(where: { original.value(forHTTPHeaderField: $0) != nil }),
              let origin = original.url, let target = request.url,
              let scheme = origin.scheme?.lowercased(), ["http", "https"].contains(scheme),
              scheme == target.scheme?.lowercased(),
              let host = origin.host?.lowercased(), host == target.host?.lowercased()
        else { return nil }

        let defaultPort = scheme == "https" ? 443 : 80
        guard (origin.port ?? defaultPort) == (target.port ?? defaultPort) else { return nil }
        var authenticated = request
        for header in Self.credentialHeaders {
            if let value = original.value(forHTTPHeaderField: header) {
                authenticated.setValue(value, forHTTPHeaderField: header)
            }
        }
        return authenticated
    }
}
