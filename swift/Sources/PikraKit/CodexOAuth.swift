import CryptoKit
import Foundation
import Security

/// Pending OpenAI Codex OAuth PKCE authorization request.
public struct PikraCodexAuthorization: Sendable, Equatable {
    public let url: URL
    public let state: String
    public let codeVerifier: String
    public let redirectURI: String

    public init(url: URL, state: String, codeVerifier: String, redirectURI: String) {
        self.url = url
        self.state = state
        self.codeVerifier = codeVerifier
        self.redirectURI = redirectURI
    }
}

/// Persisted OpenAI Codex / ChatGPT OAuth credentials.
public struct PikraCodexCredentials: Codable, Sendable, Equatable {
    public var accessToken: String
    public var refreshToken: String?
    public var accountID: String?
    public var expiresAt: Date?

    public init(
        accessToken: String,
        refreshToken: String? = nil,
        accountID: String? = nil,
        expiresAt: Date? = nil
    ) {
        self.accessToken = accessToken
        self.refreshToken = refreshToken
        self.accountID = accountID
        self.expiresAt = expiresAt
    }

    /// Decodes the unverified payload claims of a JWT (RFC 4648 base64url middle segment).
    public static func decodeJWTClaims(_ token: String) -> JSONValue? {
        let parts = token.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3, !parts[1].isEmpty else { return nil }
        guard let payloadData = decodeBase64URL(String(parts[1])) else { return nil }
        return try? JSONDecoder().decode(JSONValue.self, from: payloadData)
    }

    /// Account email parsed from the access token's JWT claims, if present.
    public var email: String? {
        guard let claims = Self.decodeJWTClaims(accessToken) else { return nil }
        if let profileEmail = claims["https://api.openai.com/profile"]?["email"]?.stringValue,
           !profileEmail.isEmpty
        {
            return profileEmail
        }
        if let directEmail = claims["email"]?.stringValue, !directEmail.isEmpty {
            return directEmail
        }
        return nil
    }

    /// Validates `accessToken` and fills missing `accountID` and `expiresAt` fields from JWT claims.
    public func normalized() throws -> PikraCodexCredentials {
        let trimmedAccess = accessToken.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedAccess.isEmpty else {
            throw PikraCodexOAuthError.invalidCredentials
        }

        let claims = Self.decodeJWTClaims(trimmedAccess)
        var resolvedAccountID = accountID?.trimmingCharacters(in: .whitespacesAndNewlines)
        if resolvedAccountID?.isEmpty ?? true {
            if let claimAccount = claims?["https://api.openai.com/auth"]?["chatgpt_account_id"]?.stringValue,
               !claimAccount.isEmpty
            {
                resolvedAccountID = claimAccount
            } else {
                resolvedAccountID = nil
            }
        }

        var resolvedExpiresAt = expiresAt
        if resolvedExpiresAt == nil, let exp = claims?["exp"]?.doubleValue {
            resolvedExpiresAt = Date(timeIntervalSince1970: exp)
        }

        let resolvedRefresh = refreshToken?.trimmingCharacters(in: .whitespacesAndNewlines)
        return PikraCodexCredentials(
            accessToken: trimmedAccess,
            refreshToken: (resolvedRefresh?.isEmpty ?? true) ? nil : resolvedRefresh,
            accountID: resolvedAccountID,
            expiresAt: resolvedExpiresAt
        )
    }
}

/// Errors produced by `PikraCodexOAuth` and `PikraCodexSubscription`.
public enum PikraCodexOAuthError: Error, Sendable, Equatable, LocalizedError {
    case invalidCallback
    case stateMismatch
    case invalidCredentials
    case tokenExchangeFailed(statusCode: Int, message: String)
    case refreshFailed(statusCode: Int, message: String)

    public var errorDescription: String? {
        switch self {
        case .invalidCallback:
            return "Invalid OAuth callback URL or authorization code"
        case .stateMismatch:
            return "OAuth state mismatch"
        case .invalidCredentials:
            return "Invalid or missing Codex OAuth credentials"
        case .tokenExchangeFailed(let statusCode, let message):
            return "Codex OAuth token exchange failed (HTTP \(statusCode)): \(message)"
        case .refreshFailed(let statusCode, let message):
            return "Codex OAuth token refresh failed (HTTP \(statusCode)): \(message)"
        }
    }
}

/// Pure-Swift 6 OpenAI Codex OAuth PKCE client using `CryptoKit`, `Security`, and `PikraHTTPClient`.
public struct PikraCodexOAuth: Sendable {
    public static let clientID = "app_EMoamEEZ73f0CkXaXp7hrann"
    public static let authorizeURL = "https://auth.openai.com/oauth/authorize"
    public static let tokenURL = "https://auth.openai.com/oauth/token"
    public static let defaultRedirectURI = "http://localhost:1455/auth/callback"
    public static let codexBaseURL = "https://chatgpt.com/backend-api"
    public static let callbackPort: UInt16 = 1455
    public static let callbackPath = "/auth/callback"

    private static let scope = "openid profile email offline_access"
    private static let originator = "pikra"
    private static let maxTokenResponseBytes = 4 * 1024 * 1024

    private let httpClient: any PikraHTTPClient
    public let redirectURI: String

    public init(
        httpClient: any PikraHTTPClient = PikraURLSessionHTTPClient(),
        redirectURI: String = Self.defaultRedirectURI
    ) {
        self.httpClient = httpClient
        self.redirectURI = redirectURI
    }

    /// Generates a PKCE verifier, S256 challenge, random state, and OpenAI authorization URL.
    public func startAuthorization() throws -> PikraCodexAuthorization {
        let verifierBytes = try randomBytes(count: 32)
        let codeVerifier = encodeBase64URL(verifierBytes)
        let digest = SHA256.hash(data: Data(codeVerifier.utf8))
        let codeChallenge = encodeBase64URL(Data(digest))
        let stateBytes = try randomBytes(count: 16)
        let state = stateBytes.map { String(format: "%02x", $0) }.joined()

        guard var components = URLComponents(string: Self.authorizeURL) else {
            throw PikraCodexOAuthError.invalidCallback
        }
        components.queryItems = [
            URLQueryItem(name: "response_type", value: "code"),
            URLQueryItem(name: "client_id", value: Self.clientID),
            URLQueryItem(name: "redirect_uri", value: redirectURI),
            URLQueryItem(name: "scope", value: Self.scope),
            URLQueryItem(name: "code_challenge", value: codeChallenge),
            URLQueryItem(name: "code_challenge_method", value: "S256"),
            URLQueryItem(name: "state", value: state),
            URLQueryItem(name: "id_token_add_organizations", value: "true"),
            URLQueryItem(name: "codex_cli_simplified_flow", value: "true"),
            URLQueryItem(name: "originator", value: Self.originator),
        ]
        guard let url = components.url else {
            throw PikraCodexOAuthError.invalidCallback
        }

        return PikraCodexAuthorization(
            url: url,
            state: state,
            codeVerifier: codeVerifier,
            redirectURI: redirectURI
        )
    }

    /// Completes authorization from a full redirect URL, `code#state`, query string, or raw code.
    public func completeAuthorization(
        _ authorization: PikraCodexAuthorization,
        callback: String
    ) async throws -> PikraCodexCredentials {
        let parsed = parseCallbackInput(callback)
        if let returnedState = parsed.state, !returnedState.isEmpty, returnedState != authorization.state {
            throw PikraCodexOAuthError.stateMismatch
        }
        guard let code = parsed.code, !code.isEmpty else {
            throw PikraCodexOAuthError.invalidCallback
        }

        let formBody = encodeFormURL([
            ("grant_type", "authorization_code"),
            ("client_id", Self.clientID),
            ("code", code),
            ("code_verifier", authorization.codeVerifier),
            ("redirect_uri", authorization.redirectURI),
        ])
        let tokenResponse = try await postTokenRequest(body: formBody, isRefresh: false)
        let expiresAt = tokenResponse.expiresIn.map { Date().addingTimeInterval($0) }
        return try PikraCodexCredentials(
            accessToken: tokenResponse.accessToken,
            refreshToken: tokenResponse.refreshToken,
            accountID: nil,
            expiresAt: expiresAt
        ).normalized()
    }

    /// Exchanges a refresh token for fresh credentials, preserving omitted `refreshToken`/`accountID`.
    public func refreshCredentials(
        _ credentials: PikraCodexCredentials
    ) async throws -> PikraCodexCredentials {
        guard let refreshToken = credentials.refreshToken?.trimmingCharacters(in: .whitespacesAndNewlines),
              !refreshToken.isEmpty
        else {
            throw PikraCodexOAuthError.invalidCredentials
        }

        let formBody = encodeFormURL([
            ("grant_type", "refresh_token"),
            ("refresh_token", refreshToken),
            ("client_id", Self.clientID),
        ])
        let tokenResponse = try await postTokenRequest(body: formBody, isRefresh: true)
        let expiresAt = tokenResponse.expiresIn.map { Date().addingTimeInterval($0) }
        let nextRefresh = tokenResponse.refreshToken.flatMap { $0.isEmpty ? nil : $0 } ?? refreshToken

        var updated = try PikraCodexCredentials(
            accessToken: tokenResponse.accessToken,
            refreshToken: nextRefresh,
            accountID: nil,
            expiresAt: expiresAt
        ).normalized()

        if updated.accountID == nil,
           let preservedAccount = credentials.accountID,
           !preservedAccount.isEmpty
        {
            updated.accountID = preservedAccount
        }
        return updated
    }

    // MARK: - Private Helpers

    private struct TokenPayload: Decodable {
        let accessToken: String?
        let refreshToken: String?
        let expiresIn: Double?

        enum CodingKeys: String, CodingKey {
            case accessToken = "access_token"
            case refreshToken = "refresh_token"
            case expiresIn = "expires_in"
        }
    }

    private struct ValidatedTokenPayload {
        let accessToken: String
        let refreshToken: String?
        let expiresIn: Double?
    }

    private func postTokenRequest(body: Data, isRefresh: Bool) async throws -> ValidatedTokenPayload {
        guard let url = URL(string: Self.tokenURL) else {
            throw isRefresh
                ? PikraCodexOAuthError.refreshFailed(statusCode: 0, message: "Invalid token URL")
                : PikraCodexOAuthError.tokenExchangeFailed(statusCode: 0, message: "Invalid token URL")
        }

        let request = PikraHTTPRequest(
            method: "POST",
            url: url,
            headers: [
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Accept", "application/json"),
            ],
            body: body,
            timeoutInterval: 30
        )

        let response = try await httpClient.send(request)
        defer { response.cancel() }

        var rawBody = Data()
        for try await chunk in response.body {
            rawBody.append(chunk)
            if rawBody.count > Self.maxTokenResponseBytes {
                let message = "Token response exceeded maximum size"
                throw isRefresh
                    ? PikraCodexOAuthError.refreshFailed(statusCode: response.statusCode, message: message)
                    : PikraCodexOAuthError.tokenExchangeFailed(statusCode: response.statusCode, message: message)
            }
        }

        let bodyText = String(data: rawBody, encoding: .utf8) ?? ""
        guard (200..<300).contains(response.statusCode) else {
            throw isRefresh
                ? PikraCodexOAuthError.refreshFailed(statusCode: response.statusCode, message: bodyText)
                : PikraCodexOAuthError.tokenExchangeFailed(statusCode: response.statusCode, message: bodyText)
        }

        guard let payload = try? JSONDecoder().decode(TokenPayload.self, from: rawBody),
              let accessToken = payload.accessToken?.trimmingCharacters(in: .whitespacesAndNewlines),
              !accessToken.isEmpty
        else {
            let message = bodyText.isEmpty ? "Missing access_token in token response" : bodyText
            throw isRefresh
                ? PikraCodexOAuthError.refreshFailed(statusCode: response.statusCode, message: message)
                : PikraCodexOAuthError.tokenExchangeFailed(statusCode: response.statusCode, message: message)
        }

        return ValidatedTokenPayload(
            accessToken: accessToken,
            refreshToken: payload.refreshToken,
            expiresIn: payload.expiresIn
        )
    }

    private func parseCallbackInput(_ input: String) -> (code: String?, state: String?) {
        let value = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return (nil, nil) }

        if let components = URLComponents(string: value), components.scheme != nil {
            let code = components.queryItems?.first(where: { $0.name == "code" })?.value
            let state = components.queryItems?.first(where: { $0.name == "state" })?.value
            return (code, state)
        }

        if let hashIndex = value.firstIndex(of: "#") {
            let code = String(value[..<hashIndex])
            let state = String(value[value.index(after: hashIndex)...])
            return (code.isEmpty ? nil : code, state.isEmpty ? nil : state)
        }

        if value.contains("code=") {
            let query = value.hasPrefix("?") ? String(value.dropFirst()) : value
            if let components = URLComponents(string: "http://localhost?\(query)") {
                let code = components.queryItems?.first(where: { $0.name == "code" })?.value
                let state = components.queryItems?.first(where: { $0.name == "state" })?.value
                return (code, state)
            }
        }

        return (value, nil)
    }
}

/// Shared OpenAI Codex subscription manager that coalesces concurrent refreshes and exposes `requestAuthorizer`.
public actor PikraCodexSubscription {
    private static let refreshLeewaySeconds: TimeInterval = 300

    private let loadCredentials: @Sendable () async throws -> PikraCodexCredentials
    private let onRefresh: @Sendable (PikraCodexCredentials) async throws -> Void
    private let oauth: PikraCodexOAuth

    private var cached: PikraCodexCredentials?
    private var inFlightRefresh: Task<PikraCodexCredentials, any Error>?

    public init(
        credentials: @escaping @Sendable () async throws -> PikraCodexCredentials,
        onRefresh: @escaping @Sendable (PikraCodexCredentials) async throws -> Void,
        httpClient: any PikraHTTPClient = PikraURLSessionHTTPClient()
    ) {
        self.loadCredentials = credentials
        self.onRefresh = onRefresh
        self.oauth = PikraCodexOAuth(httpClient: httpClient)
    }

    /// Returns valid credentials, automatically refreshing within 300 seconds of expiry or when forced.
    /// Concurrent callers coalesce onto a single in-flight refresh request.
    public func currentCredentials(forceRefresh: Bool = false) async throws -> PikraCodexCredentials {
        if let active = inFlightRefresh {
            return try await active.value
        }

        let current: PikraCodexCredentials
        if let cached {
            current = cached
        } else {
            let loaded = try await loadCredentials().normalized()
            if let active = inFlightRefresh {
                return try await active.value
            }
            if let updated = self.cached {
                current = updated
            } else {
                self.cached = loaded
                current = loaded
            }
        }

        let isExpiring = current.expiresAt.map { $0.timeIntervalSinceNow <= Self.refreshLeewaySeconds } ?? false
        let shouldRefresh = forceRefresh || (isExpiring && current.refreshToken != nil)
        guard shouldRefresh else {
            return current
        }

        if let active = inFlightRefresh {
            return try await active.value
        }

        let oauth = self.oauth
        let onRefresh = self.onRefresh
        let refreshTask = Task<PikraCodexCredentials, any Error> {
            let refreshed = try await oauth.refreshCredentials(current)
            try await onRefresh(refreshed)
            return refreshed
        }
        inFlightRefresh = refreshTask
        defer { inFlightRefresh = nil }

        let refreshed = try await refreshTask.value
        cached = refreshed
        return refreshed
    }

    /// Per-request authorizer compatible with `PikraConfiguration.authorizeRequest`.
    public nonisolated var requestAuthorizer: PikraRequestAuthorizer {
        { [self] request in
            let creds = try await self.currentCredentials()
            request.setValue("Bearer \(creds.accessToken)", forHTTPHeaderField: "Authorization")
            if let accountID = creds.accountID, !accountID.isEmpty {
                request.setValue(accountID, forHTTPHeaderField: "chatgpt-account-id")
            }
            request.setValue("responses=experimental", forHTTPHeaderField: "OpenAI-Beta")
            request.setValue("pikra", forHTTPHeaderField: "originator")
        }
    }
}

// MARK: - Encoding & Random Helpers

private func randomBytes(count: Int) throws -> Data {
    var bytes = Data(count: count)
    let status = bytes.withUnsafeMutableBytes { buf -> OSStatus in
        guard let base = buf.baseAddress else { return errSecAllocate }
        return SecRandomCopyBytes(kSecRandomDefault, count, base)
    }
    guard status == errSecSuccess else {
        throw PikraError.internalError("SecRandomCopyBytes failed with status \(status)")
    }
    return bytes
}

private func encodeBase64URL(_ data: Data) -> String {
    data.base64EncodedString()
        .replacingOccurrences(of: "+", with: "-")
        .replacingOccurrences(of: "/", with: "_")
        .replacingOccurrences(of: "=", with: "")
}

private func decodeBase64URL(_ input: String) -> Data? {
    let rem = input.count % 4
    guard rem != 1 else { return nil }
    var base64 = input
        .replacingOccurrences(of: "-", with: "+")
        .replacingOccurrences(of: "_", with: "/")
    if rem != 0 {
        base64 += String(repeating: "=", count: 4 - rem)
    }
    return Data(base64Encoded: base64)
}

private let formURLAllowedCharacters = CharacterSet(
    charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~"
)

private func encodeFormURL(_ pairs: [(String, String)]) -> Data {
    let body = pairs.map { key, value in
        let k = key.addingPercentEncoding(withAllowedCharacters: formURLAllowedCharacters) ?? key
        let v = value.addingPercentEncoding(withAllowedCharacters: formURLAllowedCharacters) ?? value
        return "\(k)=\(v)"
    }.joined(separator: "&")
    return Data(body.utf8)
}
