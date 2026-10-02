// generated from the fenecdb schema: schema.fenecql
// fenec types --lang swift schema.fenecql > FenecSchema.swift
// Do not edit by hand -- regenerate when the schema changes.

import Foundation

/// `json`: any value JSON holds.
public indirect enum JSON: Codable, Sendable, Equatable {
    case null, bool(Bool), number(Double), string(String), array([JSON]), object([String: JSON])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSON].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSON].self)) }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }
}

/// A row of `articles`.
public struct Articles: Codable, Sendable, Equatable {
    public var id: Int64
    public var title: String  // text collate tr @text(k1=0.9, b=0.4, prefix=6, chars) required
    public var year: Int64?  // int @hash
    public var score: Double?  // float @sorted
    public var draft: Bool  // bool required
    public var published: String?  // timestamp @ttl(30d)
    public var cover: [UInt8]?  // bytes
    public var meta: JSON?  // json
    public var embed: [Float]?  // vector<384> @hnsw(cosine, m=8, ef_construction=200, ef_search=100)
    public var small: [Float]?  // vector<4, f16> @hnsw(l2, m=16, ef_construction=200, ef_search=100, quant=int8)
    public var splade: String?  // sparse<30522> @inverted
    public var tags: [String]?  // [text] @hash
    public var counts: [Int64]  // [int] required
    public var slug: String?  // text @unique
}

/// A row of `product_reviews`.
public struct ProductReviews: Codable, Sendable, Equatable {
    public var id: Int64
    public var productId: Int64  // int @hash required
    public var stars: Int64?  // int
    public var notes: String?  // text collate und

    enum CodingKeys: String, CodingKey {
        case id
        case productId = "product_id"
        case stars
        case notes
    }
}

/// A row of `kişiler`.
public struct Kişiler: Codable, Sendable, Equatable {
    public var id: Int64
    public var ad: String?  // text
    public var yaş: Int64?  // int
}

/// A row of `text`.
public struct Text: Codable, Sendable, Equatable {
    public var id: Int64
    public var body: String?  // text
}
