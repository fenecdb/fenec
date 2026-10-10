// generated from the fenecdb schema: schema.fenecql
// fenec types --lang kotlin schema.fenecql > FenecSchema.kt
// Do not edit by hand -- regenerate when the schema changes.

package fenecschema

import com.fenecdb.Row

/** A row of `articles`. */
data class Articles(
    val id: Long,
    val title: String, // text collate tr @text(k1=0.9, b=0.4, prefix=6, chars) required
    val year: Long?, // int @hash
    val score: Double?, // float @sorted
    val draft: Boolean, // bool required
    val published: String?, // timestamp @ttl(30d)
    val cover: List<Int>?, // bytes
    val meta: Any?, // json
    val loc: List<Double>?, // geo @geo
    val embed: List<Float>?, // vector<384> @hnsw(cosine, m=8, ef_construction=200, ef_search=100)
    val small: List<Float>?, // vector<4, f16> @hnsw(l2, m=16, ef_construction=200, ef_search=100, quant=int8)
    val splade: String?, // sparse<30522> @inverted
    val tags: List<String>?, // [text] @hash
    val counts: List<Long>, // [int] required
    val slug: String?, // text @unique
) {
    companion object {
        /** The row as the binding reads it. */
        fun from(row: Row) = Articles(
            id = (row["id"] as Number).toLong(),
            title = row["title"]!!.let { v -> (v as String) },
            year = row["year"]?.let { v -> (v as Number).toLong() },
            score = row["score"]?.let { v -> (v as Number).toDouble() },
            draft = row["draft"]!!.let { v -> (v as Boolean) },
            published = row["published"]?.let { v -> (v as String) },
            cover = row["cover"]?.let { v -> (v as List<*>).map { (it as Number).toInt() } },
            meta = row["meta"]?.let { v -> v },
            loc = row["loc"]?.let { v -> (v as List<*>).map { (it as Number).toDouble() } },
            embed = row["embed"]?.let { v -> (v as List<*>).map { (it as Number).toFloat() } },
            small = row["small"]?.let { v -> (v as List<*>).map { (it as Number).toFloat() } },
            splade = row["splade"]?.let { v -> (v as String) },
            tags = row["tags"]?.let { v -> (v as List<*>).map { v -> (v as String) } },
            counts = row["counts"]!!.let { v -> (v as List<*>).map { v -> (v as Number).toLong() } },
            slug = row["slug"]?.let { v -> (v as String) },
        )
    }
}

/** A row of `product_reviews`. */
data class ProductReviews(
    val id: Long,
    val productId: Long, // int @hash required
    val stars: Long?, // int
    val notes: String?, // text collate und
) {
    companion object {
        /** The row as the binding reads it. */
        fun from(row: Row) = ProductReviews(
            id = (row["id"] as Number).toLong(),
            productId = row["product_id"]!!.let { v -> (v as Number).toLong() },
            stars = row["stars"]?.let { v -> (v as Number).toLong() },
            notes = row["notes"]?.let { v -> (v as String) },
        )
    }
}

/** A row of `kişiler`. */
data class Kişiler(
    val id: Long,
    val ad: String?, // text
    val yaş: Long?, // int
) {
    companion object {
        /** The row as the binding reads it. */
        fun from(row: Row) = Kişiler(
            id = (row["id"] as Number).toLong(),
            ad = row["ad"]?.let { v -> (v as String) },
            yaş = row["yaş"]?.let { v -> (v as Number).toLong() },
        )
    }
}

/** A row of `text`. */
data class Text(
    val id: Long,
    val body: String?, // text
) {
    companion object {
        /** The row as the binding reads it. */
        fun from(row: Row) = Text(
            id = (row["id"] as Number).toLong(),
            body = row["body"]?.let { v -> (v as String) },
        )
    }
}
