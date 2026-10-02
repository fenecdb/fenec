import Foundation

/// An `Encodable` value as a `Value`, its keys in the order it encodes them
/// -- a synthesized `Encodable` encodes its properties as declared, so a
/// `struct Todo` writes `put todos {title: $1, done: $2}` in its own order.
/// `JSONEncoder` was not used: what order its object's keys come out in is
/// its own business, and the text a builder writes should not depend on it.
/// A `Date` is the ISO text the builder writes one as, a `[Float]` a vector.
enum ValueEncoder {
    static func encode<T: Encodable>(_ value: T) throws -> Value {
        if let v = try special(value) { return v }
        let box = Box()
        try value.encode(to: Node(box: box, codingPath: []))
        return box.resolved
    }

    static func special(_ value: Any) throws -> Value? {
        switch value {
        case let v as Value: return v
        case let r as Row: return .object(r)
        case let d as Date: return .string(iso(d))
        case let f as [Float]: return .floats(f)
        default: return nil
        }
    }

    /// What a container writes into: a value, a list or an object, filled in
    /// as the encoding goes.
    final class Box {
        var value: Value = .null
        var list: [Box]?
        var fields: [(String, Box)]?

        var resolved: Value {
            if let list { return .array(list.map(\.resolved)) }
            if let fields { return .object(Row(fields.map { ($0.0, $0.1.resolved) })) }
            return value
        }
    }

    struct Node: Encoder {
        let box: Box
        var codingPath: [CodingKey]
        var userInfo: [CodingUserInfoKey: Any] { [:] }

        func container<Key: CodingKey>(keyedBy type: Key.Type) -> KeyedEncodingContainer<Key> {
            if box.fields == nil { box.fields = [] }
            return KeyedEncodingContainer(Keyed<Key>(box: box, codingPath: codingPath))
        }

        func unkeyedContainer() -> UnkeyedEncodingContainer {
            if box.list == nil { box.list = [] }
            return Unkeyed(box: box, codingPath: codingPath)
        }

        func singleValueContainer() -> SingleValueEncodingContainer {
            Single(box: box, codingPath: codingPath)
        }
    }

    /// Encodes `value` into `box`: through its own `encode(to:)`, or as
    /// what the builder takes it for.
    static func put<T: Encodable>(_ value: T, into box: Box, path: [CodingKey]) throws {
        if let v = try special(value) {
            box.value = v
            return
        }
        try value.encode(to: Node(box: box, codingPath: path))
        box.value = box.resolved
        box.list = nil
        box.fields = nil
    }

    struct Keyed<Key: CodingKey>: KeyedEncodingContainerProtocol {
        let box: Box
        var codingPath: [CodingKey]

        func child(_ key: Key) -> Box {
            let b = Box()
            if let i = box.fields?.firstIndex(where: { $0.0 == key.stringValue }) {
                box.fields?[i].1 = b
            } else {
                box.fields?.append((key.stringValue, b))
            }
            return b
        }

        mutating func encodeNil(forKey key: Key) throws { child(key).value = .null }
        mutating func encode<T: Encodable>(_ value: T, forKey key: Key) throws {
            try ValueEncoder.put(value, into: child(key), path: codingPath + [key])
        }
        mutating func nestedContainer<N: CodingKey>(keyedBy: N.Type, forKey key: Key) -> KeyedEncodingContainer<N> {
            let b = child(key)
            b.fields = []
            return KeyedEncodingContainer(Keyed<N>(box: b, codingPath: codingPath + [key]))
        }
        mutating func nestedUnkeyedContainer(forKey key: Key) -> UnkeyedEncodingContainer {
            let b = child(key)
            b.list = []
            return Unkeyed(box: b, codingPath: codingPath + [key])
        }
        mutating func superEncoder() -> Encoder { Node(box: box, codingPath: codingPath) }
        mutating func superEncoder(forKey key: Key) -> Encoder { Node(box: child(key), codingPath: codingPath + [key]) }
    }

    struct Unkeyed: UnkeyedEncodingContainer {
        let box: Box
        var codingPath: [CodingKey]
        var count: Int { box.list?.count ?? 0 }

        func next() -> Box {
            let b = Box()
            box.list?.append(b)
            return b
        }

        mutating func encodeNil() throws { next().value = .null }
        mutating func encode<T: Encodable>(_ value: T) throws {
            try ValueEncoder.put(value, into: next(), path: codingPath)
        }
        mutating func nestedContainer<N: CodingKey>(keyedBy: N.Type) -> KeyedEncodingContainer<N> {
            let b = next()
            b.fields = []
            return KeyedEncodingContainer(Keyed<N>(box: b, codingPath: codingPath))
        }
        mutating func nestedUnkeyedContainer() -> UnkeyedEncodingContainer {
            let b = next()
            b.list = []
            return Unkeyed(box: b, codingPath: codingPath)
        }
        mutating func superEncoder() -> Encoder { Node(box: next(), codingPath: codingPath) }
    }

    struct Single: SingleValueEncodingContainer {
        let box: Box
        var codingPath: [CodingKey]

        mutating func encodeNil() throws { box.value = .null }
        mutating func encode(_ value: Bool) throws { box.value = .bool(value) }
        mutating func encode(_ value: String) throws { box.value = .string(value) }
        mutating func encode(_ value: Double) throws { box.value = .double(value) }
        mutating func encode(_ value: Float) throws { box.value = .double(Double(value)) }
        mutating func encode(_ value: Int) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: Int8) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: Int16) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: Int32) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: Int64) throws { box.value = .int(value) }
        mutating func encode(_ value: UInt) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: UInt8) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: UInt16) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: UInt32) throws { box.value = .int(Int64(value)) }
        mutating func encode(_ value: UInt64) throws { box.value = .int(Int64(value)) }
        mutating func encode<T: Encodable>(_ value: T) throws {
            try ValueEncoder.put(value, into: box, path: codingPath)
        }
    }
}
