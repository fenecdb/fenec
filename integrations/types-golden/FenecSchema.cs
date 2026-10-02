// generated from the fenecdb schema: schema.fenecql
// fenec types --lang csharp schema.fenecql > FenecSchema.cs
// Do not edit by hand -- regenerate when the schema changes.

#nullable enable
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FenecSchema;

/// <summary>A row of <c>articles</c>.</summary>
public sealed record Articles(
    [property: JsonPropertyName("id")] long Id,
    /* text collate tr @text(k1=0.9, b=0.4, prefix=6, chars) required */ [property: JsonPropertyName("title")] string Title,
    /* int @hash */ [property: JsonPropertyName("year")] long? Year,
    /* float @sorted */ [property: JsonPropertyName("score")] double? Score,
    /* bool required */ [property: JsonPropertyName("draft")] bool Draft,
    /* timestamp @ttl(30d) */ [property: JsonPropertyName("published")] string? Published,
    /* bytes */ [property: JsonPropertyName("cover")] int[]? Cover,
    /* json */ [property: JsonPropertyName("meta")] JsonElement? Meta,
    /* vector<384> @hnsw(cosine, m=8, ef_construction=200, ef_search=100) */ [property: JsonPropertyName("embed")] float[]? Embed,
    /* vector<4, f16> @hnsw(l2, m=16, ef_construction=200, ef_search=100, quant=int8) */ [property: JsonPropertyName("small")] float[]? Small,
    /* sparse<30522> @inverted */ [property: JsonPropertyName("splade")] string? Splade,
    /* [text] @hash */ [property: JsonPropertyName("tags")] string[]? Tags,
    /* [int] required */ [property: JsonPropertyName("counts")] long[] Counts,
    /* text @unique */ [property: JsonPropertyName("slug")] string? Slug);

/// <summary>A row of <c>product_reviews</c>.</summary>
public sealed record ProductReviews(
    [property: JsonPropertyName("id")] long Id,
    /* int @hash required */ [property: JsonPropertyName("product_id")] long ProductId,
    /* int */ [property: JsonPropertyName("stars")] long? Stars,
    /* text collate und */ [property: JsonPropertyName("notes")] string? Notes);

/// <summary>A row of <c>kişiler</c>.</summary>
public sealed record Kişiler(
    [property: JsonPropertyName("id")] long Id,
    /* text */ [property: JsonPropertyName("ad")] string? Ad,
    /* int */ [property: JsonPropertyName("yaş")] long? Yaş);

/// <summary>A row of <c>text</c>.</summary>
public sealed record Text(
    [property: JsonPropertyName("id")] long Id,
    /* text */ [property: JsonPropertyName("body")] string? Body);
