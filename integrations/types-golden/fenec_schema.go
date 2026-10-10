// generated from the fenecdb schema: schema.fenecql
// fenec types --lang go schema.fenecql > fenec_schema.go
// Do not edit by hand -- regenerate when the schema changes.

package fenecschema

import "encoding/json"

// Articles is a row of `articles`.
type Articles struct {
	ID        int64           `json:"id"`
	Title     string          `json:"title"`     // text collate tr @text(k1=0.9, b=0.4, prefix=6, chars) required
	Year      *int64          `json:"year"`      // int @hash
	Score     *float64        `json:"score"`     // float @sorted
	Draft     bool            `json:"draft"`     // bool required
	Published *string         `json:"published"` // timestamp @ttl(30d)
	Cover     []int           `json:"cover"`     // bytes
	Meta      json.RawMessage `json:"meta"`      // json
	Loc       *[2]float64     `json:"loc"`       // geo @geo
	Embed     []float32       `json:"embed"`     // vector<384> @hnsw(cosine, m=8, ef_construction=200, ef_search=100)
	Small     []float32       `json:"small"`     // vector<4, f16> @hnsw(l2, m=16, ef_construction=200, ef_search=100, quant=int8)
	Splade    *string         `json:"splade"`    // sparse<30522> @inverted
	Tags      []string        `json:"tags"`      // [text] @hash
	Counts    []int64         `json:"counts"`    // [int] required
	Slug      *string         `json:"slug"`      // text @unique
}

// ProductReviews is a row of `product_reviews`.
type ProductReviews struct {
	ID        int64   `json:"id"`
	ProductId int64   `json:"product_id"` // int @hash required
	Stars     *int64  `json:"stars"`      // int
	Notes     *string `json:"notes"`      // text collate und
}

// Kişiler is a row of `kişiler`.
type Kişiler struct {
	ID  int64   `json:"id"`
	Ad  *string `json:"ad"`  // text
	Yaş *int64  `json:"yaş"` // int
}

// Text is a row of `text`.
type Text struct {
	ID   int64   `json:"id"`
	Body *string `json:"body"` // text
}
