// generated from the fenecdb schema: schema.fenecql
// fenec types --lang dart schema.fenecql > fenec_schema.dart
// Do not edit by hand -- regenerate when the schema changes.

/// A row of `articles`.
class Articles {
  final int id;
  /// text collate tr @text(k1=0.9, b=0.4, prefix=6, chars) required
  final String title;
  /// int @hash
  final int? year;
  /// float @sorted
  final double? score;
  /// bool required
  final bool draft;
  /// timestamp @ttl(30d)
  final String? published;
  /// bytes
  final List<int>? cover;
  /// json
  final Object? meta;
  /// vector<384> @hnsw(cosine, m=8, ef_construction=200, ef_search=100)
  final List<double>? embed;
  /// vector<4, f16> @hnsw(l2, m=16, ef_construction=200, ef_search=100, quant=int8)
  final List<double>? small;
  /// sparse<30522> @inverted
  final String? splade;
  /// [text] @hash
  final List<String>? tags;
  /// [int] required
  final List<int> counts;
  /// text @unique
  final String? slug;

  const Articles({required this.id, required this.title, this.year, this.score, required this.draft, this.published, this.cover, this.meta, this.embed, this.small, this.splade, this.tags, required this.counts, this.slug});

  /// The row as JSON reads it.
  factory Articles.fromJson(Map<String, Object?> j) => Articles(
        id: j['id'] as int,
        title: switch (j['title']) { final v => v as String },
        year: switch (j['year']) { null => null, final v => v as int },
        score: switch (j['score']) { null => null, final v => (v as num).toDouble() },
        draft: switch (j['draft']) { final v => v as bool },
        published: switch (j['published']) { null => null, final v => v as String },
        cover: switch (j['cover']) { null => null, final v => [for (final e in v as List) e as int] },
        meta: switch (j['meta']) { null => null, final v => v },
        embed: switch (j['embed']) { null => null, final v => [for (final e in v as List) (e as num).toDouble()] },
        small: switch (j['small']) { null => null, final v => [for (final e in v as List) (e as num).toDouble()] },
        splade: switch (j['splade']) { null => null, final v => v as String },
        tags: switch (j['tags']) { null => null, final v => [for (final v in v as List) v as String] },
        counts: switch (j['counts']) { final v => [for (final v in v as List) v as int] },
        slug: switch (j['slug']) { null => null, final v => v as String },
      );
}

/// A row of `product_reviews`.
class ProductReviews {
  final int id;
  /// int @hash required
  final int productId;
  /// int
  final int? stars;
  /// text collate und
  final String? notes;

  const ProductReviews({required this.id, required this.productId, this.stars, this.notes});

  /// The row as JSON reads it.
  factory ProductReviews.fromJson(Map<String, Object?> j) => ProductReviews(
        id: j['id'] as int,
        productId: switch (j['product_id']) { final v => v as int },
        stars: switch (j['stars']) { null => null, final v => v as int },
        notes: switch (j['notes']) { null => null, final v => v as String },
      );
}

/// A row of `kişiler`.
class Ki_iler {
  final int id;
  /// text
  final String? ad;
  /// int
  final int? ya_;

  const Ki_iler({required this.id, this.ad, this.ya_});

  /// The row as JSON reads it.
  factory Ki_iler.fromJson(Map<String, Object?> j) => Ki_iler(
        id: j['id'] as int,
        ad: switch (j['ad']) { null => null, final v => v as String },
        ya_: switch (j['yaş']) { null => null, final v => v as int },
      );
}

/// A row of `text`.
class Text {
  final int id;
  /// text
  final String? body;

  const Text({required this.id, this.body});

  /// The row as JSON reads it.
  factory Text.fromJson(Map<String, Object?> j) => Text(
        id: j['id'] as int,
        body: switch (j['body']) { null => null, final v => v as String },
      );
}
