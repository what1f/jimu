import Foundation

/// A Sendable JSON value that crosses Swift 6 concurrency boundaries safely.
public enum JSONValue: Codable, Sendable, Equatable, Hashable {
    case null
    case bool(Bool)
    case integer(Int64)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: any Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let bool = try? container.decode(Bool.self) {
            self = .bool(bool)
        } else if let intVal = try? container.decode(Int64.self) {
            self = .integer(intVal)
        } else if let number = try? container.decode(Double.self) {
            self = .number(number)
        } else if let string = try? container.decode(String.self) {
            self = .string(string)
        } else if let array = try? container.decode([JSONValue].self) {
            self = .array(array)
        } else {
            self = .object(try container.decode([String: JSONValue].self))
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null:
            try container.encodeNil()
        case .bool(let value):
            try container.encode(value)
        case .integer(let value):
            try container.encode(value)
        case .number(let value):
            try container.encode(value)
        case .string(let value):
            try container.encode(value)
        case .array(let value):
            try container.encode(value)
        case .object(let value):
            try container.encode(value)
        }
    }

    public subscript(_ key: String) -> JSONValue? {
        guard case .object(let dict) = self else { return nil }
        return dict[key]
    }

    public subscript(_ index: Int) -> JSONValue? {
        guard case .array(let arr) = self, arr.indices.contains(index) else { return nil }
        return arr[index]
    }

    public var stringValue: String? {
        guard case .string(let s) = self else { return nil }
        return s
    }

    public var boolValue: Bool? {
        guard case .bool(let b) = self else { return nil }
        return b
    }

    public var doubleValue: Double? {
        switch self {
        case .integer(let i):
            return Double(i)
        case .number(let n):
            return n
        default:
            return nil
        }
    }

    public var int64Value: Int64? {
        switch self {
        case .integer(let i):
            return i
        case .number(let n):
            guard n.isFinite, n.rounded() == n,
                  n >= -9_007_199_254_740_992.0, n <= 9_007_199_254_740_992.0
            else { return nil }
            return Int64(n)
        default:
            return nil
        }
    }

    public var intValue: Int? {
        guard let i64 = int64Value else { return nil }
        return Int(exactly: i64)
    }

    public var arrayValue: [JSONValue]? {
        guard case .array(let arr) = self else { return nil }
        return arr
    }

    public var objectValue: [String: JSONValue]? {
        guard case .object(let dict) = self else { return nil }
        return dict
    }
}

extension JSONValue: ExpressibleByNilLiteral {
    public init(nilLiteral: ()) { self = .null }
}

extension JSONValue: ExpressibleByBooleanLiteral {
    public init(booleanLiteral value: Bool) { self = .bool(value) }
}

extension JSONValue: ExpressibleByIntegerLiteral {
    public init(integerLiteral value: Int64) { self = .integer(value) }
}

extension JSONValue: ExpressibleByFloatLiteral {
    public init(floatLiteral value: Double) { self = .number(value) }
}

extension JSONValue: ExpressibleByStringLiteral {
    public init(stringLiteral value: String) { self = .string(value) }
}

extension JSONValue: ExpressibleByArrayLiteral {
    public init(arrayLiteral elements: JSONValue...) { self = .array(elements) }
}

extension JSONValue: ExpressibleByDictionaryLiteral {
    public init(dictionaryLiteral elements: (String, JSONValue)...) {
        self = .object(Dictionary(uniqueKeysWithValues: elements))
    }
}
