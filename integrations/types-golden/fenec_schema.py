# generated from the fenecdb schema: schema.fenecql
# fenec types --lang python schema.fenecql > fenec_schema.py
# Do not edit by hand -- regenerate when the schema changes.

from typing import Any, Optional, TypedDict


class Articles(TypedDict):
    """A row of `articles`."""

    id: int
    title: str  # text collate tr @text(k1=0.9, b=0.4, prefix=6, chars) required
    year: Optional[int]  # int @hash
    score: Optional[float]  # float @sorted
    draft: bool  # bool required
    published: Optional[str]  # timestamp @ttl(30d)
    cover: Optional[list[int]]  # bytes
    meta: Any  # json
    loc: Optional[list[float]]  # geo @geo
    embed: Optional[list[float]]  # vector<384> @hnsw(cosine, m=8, ef_construction=200, ef_search=100)
    small: Optional[list[float]]  # vector<4, f16> @hnsw(l2, m=16, ef_construction=200, ef_search=100, quant=int8)
    splade: Optional[str]  # sparse<30522> @inverted
    tags: Optional[list[str]]  # [text] @hash
    counts: list[int]  # [int] required
    slug: Optional[str]  # text @unique


class ProductReviews(TypedDict):
    """A row of `product_reviews`."""

    id: int
    product_id: int  # int @hash required
    stars: Optional[int]  # int
    notes: Optional[str]  # text collate und


class Kişiler(TypedDict):
    """A row of `kişiler`."""

    id: int
    ad: Optional[str]  # text
    yaş: Optional[int]  # int


class Text(TypedDict):
    """A row of `text`."""

    id: int
    body: Optional[str]  # text
