import Foundation

/// A value as fenecdb holds it, and as JSON carries it: what a row's fields
/// are, and what a parameter becomes.
///
/// Numbers keep whether they were whole: `int` for a number written without
/// a fraction or an exponent, `double` for the rest -- the engine writes an
/// `int` field's values one way and a `float`'s the other. `floats` is a
/// vector handed over as `Float`s, which goes to the library as its bytes
/// rather than as text (`[Float]` becomes one). An object keeps its keys in
/// the order written: a document's fields are the statement's text in that
/// order, and a Swift dictionary has none.
public enum Value: Sendable, Hashable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    case string(String)
    case floats([Float])
    case array([Value])
    case object(Row)

    public var isNull: Bool { if case .null = self { return true } else { return false } }

    public var bool: Bool? { if case .bool(let b) = self { return b } else { return nil } }

    public var string: String? { if case .string(let s) = self { return s } else { return nil } }

    /// A whole number, or a `double` that is one.
    public var int: Int? {
        switch self {
        case .int(let n): return Int(exactly: n)
        case .double(let d): return Int(exactly: d)
        default: return nil
        }
    }

    /// Any number.
    public var double: Double? {
        switch self {
        case .int(let n): return Double(n)
        case .double(let d): return d
        default: return nil
        }
    }

    public var array: [Value]? {
        switch self {
        case .array(let a): return a
        case .floats(let f): return f.map { .double(Double($0)) }
        default: return nil
        }
    }

    public var object: Row? { if case .object(let r) = self { return r } else { return nil } }

    /// A list of numbers as a vector: each the `Float` nearest it, which is
    /// the `Float` the engine stored for a vector's component.
    public var floats: [Float]? {
        switch self {
        case .floats(let f): return f
        case .array(let a):
            var out: [Float] = []
            out.reserveCapacity(a.count)
            for v in a {
                guard let d = v.double else { return nil }
                out.append(Float(d))
            }
            return out
        default: return nil
        }
    }

    /// A member of an object, or an element of a list.
    public subscript(key: String) -> Value? { object?[key] }
    public subscript(index: Int) -> Value? {
        guard let a = array, a.indices.contains(index) else { return nil }
        return a[index]
    }
}

extension Value: ExpressibleByNilLiteral, ExpressibleByBooleanLiteral, ExpressibleByIntegerLiteral,
    ExpressibleByFloatLiteral, ExpressibleByStringLiteral, ExpressibleByStringInterpolation, ExpressibleByArrayLiteral,
    ExpressibleByDictionaryLiteral
{
    public init(nilLiteral: ()) { self = .null }
    public init(booleanLiteral value: Bool) { self = .bool(value) }
    public init(integerLiteral value: Int64) { self = .int(value) }
    public init(floatLiteral value: Double) { self = .double(value) }
    public init(stringLiteral value: String) { self = .string(value) }
    public init(arrayLiteral elements: Value...) { self = .array(elements) }
    /// In the order written.
    public init(dictionaryLiteral elements: (String, Value)...) { self = .object(Row(elements)) }
}

/// A row of an answer, a document to write, an object of a json field: its
/// fields in order, each found by name.
public struct Row: Sendable, Hashable, Sequence, ExpressibleByDictionaryLiteral, CustomStringConvertible {
    public private(set) var keys: [String]
    public private(set) var values: [Value]

    public init() {
        keys = []
        values = []
    }

    public init(_ fields: [(String, Value)]) {
        keys = fields.map(\.0)
        values = fields.map(\.1)
    }

    public init(dictionaryLiteral elements: (String, Value)...) { self.init(elements) }

    public var count: Int { keys.count }
    public var isEmpty: Bool { keys.isEmpty }

    /// The field's value; `nil` where the row has no such field. A field
    /// that holds JSON's null is `.null`.
    public subscript(key: String) -> Value? {
        get { keys.firstIndex(of: key).map { values[$0] } }
        set {
            if let i = keys.firstIndex(of: key) {
                if let v = newValue { values[i] = v } else { keys.remove(at: i); values.remove(at: i) }
            } else if let v = newValue {
                keys.append(key)
                values.append(v)
            }
        }
    }

    public func makeIterator() -> AnyIterator<(key: String, value: Value)> {
        var i = 0
        return AnyIterator {
            guard i < keys.count else { return nil }
            defer { i += 1 }
            return (keys[i], values[i])
        }
    }

    /// The row as `T` decodes it (`Decodable`), field by field.
    public func decode<T: Decodable>(as type: T.Type = T.self) throws -> T {
        try JSONDecoder().decode(T.self, from: Data(Value.object(self).json.utf8))
    }

    public var description: String { Value.object(self).json }
}

// ----------------------------------------------------------------- reading

extension Value {
    /// Reads JSON. Numbers without a fraction or an exponent are `int`s while
    /// they fit, and an object keeps its keys' order.
    public static func parse(_ text: String) throws -> Value {
        var reader = Reader(bytes: Array(text.utf8))
        reader.space()
        let v = try reader.value(depth: 0)
        reader.space()
        guard reader.at == reader.bytes.count else { throw reader.fail("trailing characters") }
        return v
    }

    static func parse(bytes: [UInt8]) throws -> Value {
        var reader = Reader(bytes: bytes)
        reader.space()
        return try reader.value(depth: 0)
    }
}

/// A recursive reader over the text's bytes: JSON's structure is ASCII, so
/// a string is copied between its escapes, and nesting is bounded as the
/// engine bounds it.
private struct Reader {
    let bytes: [UInt8]
    var at = 0

    init(bytes: [UInt8]) { self.bytes = bytes }

    func fail(_ why: String) -> FenecError {
        FenecError(code: .query, message: "the library answered JSON it cannot read: \(why) at \(at)")
    }

    mutating func space() {
        while at < bytes.count, [0x20, 0x0a, 0x0d, 0x09].contains(bytes[at]) { at += 1 }
    }

    mutating func literal(_ word: String, _ v: Value) throws -> Value {
        let w = Array(word.utf8)
        guard at + w.count <= bytes.count, Array(bytes[at..<at + w.count]) == w else { throw fail("a bad literal") }
        at += w.count
        return v
    }

    mutating func value(depth: Int) throws -> Value {
        guard depth < 512 else { throw fail("nesting too deep") }
        guard at < bytes.count else { throw fail("the end") }
        switch bytes[at] {
        case UInt8(ascii: "n"): return try literal("null", .null)
        case UInt8(ascii: "t"): return try literal("true", .bool(true))
        case UInt8(ascii: "f"): return try literal("false", .bool(false))
        case UInt8(ascii: "\""): return .string(try string())
        case UInt8(ascii: "["):
            at += 1
            var items: [Value] = []
            space()
            if at < bytes.count, bytes[at] == UInt8(ascii: "]") { at += 1; return .array(items) }
            while true {
                space()
                items.append(try value(depth: depth + 1))
                space()
                guard at < bytes.count else { throw fail("an unclosed list") }
                if bytes[at] == UInt8(ascii: ",") { at += 1; continue }
                if bytes[at] == UInt8(ascii: "]") { at += 1; return .array(items) }
                throw fail("a list")
            }
        case UInt8(ascii: "{"):
            at += 1
            var fields: [(String, Value)] = []
            space()
            if at < bytes.count, bytes[at] == UInt8(ascii: "}") { at += 1; return .object(Row(fields)) }
            while true {
                space()
                guard at < bytes.count, bytes[at] == UInt8(ascii: "\"") else { throw fail("a key") }
                let key = try string()
                space()
                guard at < bytes.count, bytes[at] == UInt8(ascii: ":") else { throw fail("a colon") }
                at += 1
                space()
                fields.append((key, try value(depth: depth + 1)))
                space()
                guard at < bytes.count else { throw fail("an unclosed object") }
                if bytes[at] == UInt8(ascii: ",") { at += 1; continue }
                if bytes[at] == UInt8(ascii: "}") { at += 1; return .object(Row(fields)) }
                throw fail("an object")
            }
        default:
            return try number()
        }
    }

    mutating func number() throws -> Value {
        let start = at
        var whole = true
        while at < bytes.count, Reader.numberByte(bytes[at]) {
            if bytes[at] == UInt8(ascii: ".") || bytes[at] == UInt8(ascii: "e") || bytes[at] == UInt8(ascii: "E") {
                whole = false
            }
            at += 1
        }
        guard at > start, let text = String(bytes: bytes[start..<at], encoding: .utf8) else { throw fail("a value") }
        if whole, let n = Int64(text) { return .int(n) }
        guard let d = Double(text) else { throw fail("a number") }
        return .double(d)
    }

    static func numberByte(_ b: UInt8) -> Bool {
        (b >= UInt8(ascii: "0") && b <= UInt8(ascii: "9")) || b == UInt8(ascii: "-") || b == UInt8(ascii: "+")
            || b == UInt8(ascii: ".") || b == UInt8(ascii: "e") || b == UInt8(ascii: "E")
    }

    mutating func hex4() throws -> UInt32 {
        guard at + 4 <= bytes.count, let s = String(bytes: bytes[at..<at + 4], encoding: .ascii),
            let n = UInt32(s, radix: 16)
        else { throw fail("an escape") }
        at += 4
        return n
    }

    mutating func string() throws -> String {
        at += 1
        var out: [UInt8] = []
        var run = at
        while true {
            guard at < bytes.count else { throw fail("an unclosed string") }
            let b = bytes[at]
            if b == UInt8(ascii: "\"") {
                out.append(contentsOf: bytes[run..<at])
                at += 1
                return String(decoding: out, as: UTF8.self)
            }
            if b != UInt8(ascii: "\\") { at += 1; continue }
            out.append(contentsOf: bytes[run..<at])
            at += 1
            guard at < bytes.count else { throw fail("an escape") }
            let e = bytes[at]
            at += 1
            switch e {
            case UInt8(ascii: "n"): out.append(0x0a)
            case UInt8(ascii: "t"): out.append(0x09)
            case UInt8(ascii: "r"): out.append(0x0d)
            case UInt8(ascii: "b"): out.append(0x08)
            case UInt8(ascii: "f"): out.append(0x0c)
            case UInt8(ascii: "u"):
                var scalar = try hex4()
                if (0xD800..<0xDC00).contains(scalar), at + 6 <= bytes.count, bytes[at] == UInt8(ascii: "\\"),
                    bytes[at + 1] == UInt8(ascii: "u")
                {
                    at += 2
                    let low = try hex4()
                    scalar = 0x10000 + ((scalar - 0xD800) << 10) + (low - 0xDC00)
                }
                out.append(contentsOf: Array(String(Character(Unicode.Scalar(scalar) ?? "\u{FFFD}")).utf8))
            default: out.append(e)
            }
            run = at
        }
    }
}

// ----------------------------------------------------------------- writing

extension Value {
    /// The value as JSON, keys in their order.
    public var json: String {
        var out = ""
        write(into: &out)
        return out
    }

    func write(into out: inout String) {
        switch self {
        case .null: out += "null"
        case .bool(let b): out += b ? "true" : "false"
        case .int(let n): out += String(n)
        case .double(let d): out += Value.number(d)
        case .string(let s): Value.quote(s, into: &out)
        case .floats(let f):
            out += "["
            for (i, x) in f.enumerated() {
                if i > 0 { out += "," }
                out += Value.number(x)
            }
            out += "]"
        case .array(let a):
            out += "["
            for (i, v) in a.enumerated() {
                if i > 0 { out += "," }
                v.write(into: &out)
            }
            out += "]"
        case .object(let r):
            out += "{"
            for (i, (k, v)) in zip(r.keys, r.values).enumerated() {
                if i > 0 { out += "," }
                Value.quote(k, into: &out)
                out += ":"
                v.write(into: &out)
            }
            out += "}"
        }
    }

    /// A `Double` as the shortest text that reads back as it; JSON has no
    /// word for a NaN or an infinity, and the engine would refuse one, so
    /// it goes as `null`, which a typed field refuses.
    static func number(_ d: Double) -> String {
        guard d.isFinite else { return "null" }
        if d == d.rounded(), abs(d) < 1e15 { return String(Int64(d)) }
        return "\(d)"
    }

    /// A `Float` as the shortest text that reads back as it through an
    /// `f64`, which is how the engine reads a number: one `Float` --
    /// 7.038531e-26 -- has a shortest text whose `f64` lies exactly between
    /// it and the next and rounds away, and goes as its `Double`'s text.
    static func number(_ f: Float) -> String {
        guard f.isFinite else { return "null" }
        if f == f.rounded(), abs(f) < 1e7 { return String(Int64(f)) }
        let short = "\(f)"
        if let back = Double(short), Float(back) == f { return short }
        return "\(Double(f))"
    }

    static func quote(_ s: String, into out: inout String) {
        out += "\""
        for u in s.unicodeScalars {
            switch u {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            case "\u{08}": out += "\\b"
            case "\u{0C}": out += "\\f"
            case _ where u.value < 0x20:
                out += "\\u" + String(repeating: "0", count: 4 - String(u.value, radix: 16).count)
                    + String(u.value, radix: 16)
            default: out.unicodeScalars.append(u)
            }
        }
        out += "\""
    }
}

// --------------------------------------------------------------- parameters

/// What can go in as a parameter, or a document's field: a `Value`, a
/// number, a text, a `Bool`, a `Date` (as the ISO text JavaScript's
/// `toISOString` writes), a `[Float]` (a vector, handed over as its
/// bytes), a list or a dictionary of them, an optional. A `Codable` type
/// takes part by saying so -- `struct Todo: Codable, FenecValue` -- and is
/// written as its encoding, its fields in the order they are encoded.
public protocol FenecValue: Sendable {
    func fenecValue() throws -> Value
}

extension FenecValue where Self: Encodable {
    public func fenecValue() throws -> Value { try ValueEncoder.encode(self) }
}

extension Value: FenecValue { public func fenecValue() throws -> Value { self } }
extension Row: FenecValue { public func fenecValue() throws -> Value { .object(self) } }
extension String: FenecValue { public func fenecValue() throws -> Value { .string(self) } }
extension Substring: FenecValue { public func fenecValue() throws -> Value { .string(String(self)) } }
extension Bool: FenecValue { public func fenecValue() throws -> Value { .bool(self) } }
extension Int: FenecValue { public func fenecValue() throws -> Value { .int(Int64(self)) } }
extension Int32: FenecValue { public func fenecValue() throws -> Value { .int(Int64(self)) } }
extension Int64: FenecValue { public func fenecValue() throws -> Value { .int(self) } }
extension UInt32: FenecValue { public func fenecValue() throws -> Value { .int(Int64(self)) } }
extension Double: FenecValue { public func fenecValue() throws -> Value { .double(self) } }
extension Float: FenecValue { public func fenecValue() throws -> Value { .double(Double(self)) } }
extension Date: FenecValue { public func fenecValue() throws -> Value { .string(iso(self)) } }

extension Optional: FenecValue where Wrapped: FenecValue {
    public func fenecValue() throws -> Value {
        switch self {
        case .none: return .null
        case .some(let w): return try w.fenecValue()
        }
    }
}

extension Array: FenecValue where Element: FenecValue {
    public func fenecValue() throws -> Value {
        // A conformance a type: `[Float]` is the vector here.
        if let f = self as? [Float] { return .floats(f) }
        return .array(try map { try $0.fenecValue() })
    }
}

extension Dictionary: FenecValue where Key == String, Value: FenecValue {
    /// Its keys sorted: a dictionary has no order, and the text written
    /// from it should be the same each time. A `Row` or a `Value` literal
    /// keeps the order written.
    public func fenecValue() throws -> FenecDB.Value {
        .object(Row(try keys.sorted().map { ($0, try self[$0]!.fenecValue()) }))
    }
}

/// A date as JavaScript's `toISOString` writes it: UTC, to the
/// millisecond, which the JS builder sends a `Date` as.
func iso(_ date: Date) -> String {
    // To the nearest millisecond: a Date holds seconds as a Double, and
    // 1 789 821 296.789 s times 1000 is a hair under its millisecond.
    let ms = Int64((date.timeIntervalSince1970 * 1000).rounded())
    let days = ms >= 0 ? ms / 86_400_000 : (ms - 86_399_999) / 86_400_000
    let rest = ms - days * 86_400_000
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468
    let era = (z >= 0 ? z : z - 146_096) / 146_097
    let doe = z - era * 146_097
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100)
    let mp = (5 * doy + 2) / 153
    let d = doy - (153 * mp + 2) / 5 + 1
    let m = mp < 10 ? mp + 3 : mp - 9
    let y = yoe + era * 400 + (m <= 2 ? 1 : 0)
    func pad(_ n: Int64, _ w: Int) -> String {
        let s = String(n)
        return String(repeating: "0", count: max(0, w - s.count)) + s
    }
    return "\(pad(y, 4))-\(pad(m, 2))-\(pad(d, 2))T\(pad(rest / 3_600_000, 2)):\(pad(rest / 60_000 % 60, 2)):"
        + "\(pad(rest / 1000 % 60, 2)).\(pad(rest % 1000, 3))Z"
}
