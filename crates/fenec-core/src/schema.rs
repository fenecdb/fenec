use crate::codec::*;
use crate::error::{Error, Result};
use crate::value::DataType;

/// Vector similarity metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    L2,
    Dot,
}

impl Metric {
    pub fn parse(s: &str) -> Option<Metric> {
        match s.to_ascii_lowercase().as_str() {
            "cosine" | "cos" => Some(Metric::Cosine),
            "l2" | "euclidean" => Some(Metric::L2),
            "dot" | "ip" => Some(Metric::Dot),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Metric::Cosine => "cosine",
            Metric::L2 => "l2",
            Metric::Dot => "dot",
        }
    }
    fn code(&self) -> u8 {
        match self {
            Metric::Cosine => 0,
            Metric::L2 => 1,
            Metric::Dot => 2,
        }
    }
    fn from_code(c: u8) -> Result<Metric> {
        Ok(match c {
            0 => Metric::Cosine,
            1 => Metric::L2,
            2 => Metric::Dot,
            _ => return Err(Error::Corrupt(format!("unknown metric {c}"))),
        })
    }
}

/// What a vector index holds of each vector (`quant=` in `@hnsw`). The
/// documents keep their vectors whole either way: a search over codes
/// orders the candidates it found again by the documents' own vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quant {
    /// The vector itself, at the field's precision.
    #[default]
    None,
    /// A byte a component, over a scale a vector: a quarter of `f32`.
    Int8,
    /// A bit a component, its sign: a thirty-second of `f32`. The signs
    /// of a unit vector, so cosine only.
    Bit,
}

impl Quant {
    pub fn parse(s: &str) -> Option<Quant> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Some(Quant::None),
            "int8" | "i8" => Some(Quant::Int8),
            "bit" | "binary" => Some(Quant::Bit),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Quant::None => "none",
            Quant::Int8 => "int8",
            Quant::Bit => "bit",
        }
    }
    pub fn code(&self) -> u8 {
        match self {
            Quant::None => 0,
            Quant::Int8 => 1,
            Quant::Bit => 2,
        }
    }
    pub fn from_code(c: u8) -> Option<Quant> {
        match c {
            0 => Some(Quant::None),
            1 => Some(Quant::Int8),
            2 => Some(Quant::Bit),
            _ => None,
        }
    }
}

/// `ef_search` for a `quant=bit` index that names none. Its codes estimate
/// distances coarsely, so the beam has to be wider to hold the true
/// neighbours for the documents' own vectors to put in order: over a million
/// clustered 768-dimension vectors a beam of 100 held 82.5% of the true ten
/// and one of 400 held 98.4%, where int8 codes held 97.1% at 100 and full
/// vectors 97.4% (`make quant-bench`).
pub const BIT_EF_SEARCH: usize = 400;

/// HNSW parameters. The defaults were picked targeting ~95% recall /
/// a few hundred microseconds on 1M-scale collections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorIndexSpec {
    pub metric: Metric,
    /// Number of connections per layer.
    pub m: usize,
    /// Candidate list width during construction.
    pub ef_construction: usize,
    /// Default candidate list width at query time.
    pub ef_search: usize,
    /// What the index holds of each vector.
    pub quant: Quant,
}

impl VectorIndexSpec {
    /// `, quant=int8` for a quantized index and nothing for the rest: how
    /// the option is written back wherever the others are.
    pub fn quant_arg(&self) -> &'static str {
        match self.quant {
            Quant::None => "",
            Quant::Int8 => ", quant=int8",
            Quant::Bit => ", quant=bit",
        }
    }
}

impl Default for VectorIndexSpec {
    fn default() -> Self {
        VectorIndexSpec {
            metric: Metric::Cosine,
            m: 16,
            ef_construction: 200,
            // recall@10 measured at 100 is 100%; at 64 it is 99%. ANN latency
            // rises from 0.10 -> 0.13 ms, which is still ~7x faster than the
            // engines we compare against. The accuracy is worth the trade.
            ef_search: 100,
            quant: Quant::None,
        }
    }
}

/// BM25 parameters.
///
/// Held in hundredths rather than as `f32` so the schema can stay `Eq`:
/// `Field` and `Schema` derive it, and `Database::load` compares decoded
/// schemas against live ones. Two tuning knobs are not worth taking that
/// away from every caller. The defaults are the pair BEIR standardised on,
/// and the numbers in `text.rs` were measured with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextIndexSpec {
    /// Term-frequency saturation, in hundredths. BEIR's k1 = 0.9 -> 90.
    pub k1_pct: u16,
    /// Length normalisation, in hundredths. BEIR's b = 0.4 -> 40.
    pub b_pct: u16,
    /// Longest word prefix also indexed as a term; 0 turns the whole thing
    /// off, which is the default.
    ///
    /// Word-boundary matching is a poor fit for a language that inflects by
    /// gluing suffixes on: `kitap`, `kitabı`, `kitapların` are three terms
    /// that share a stem the index never sees. Indexing each word's prefixes
    /// alongside it is a stemmer that needs no dictionary and no language
    /// setting.
    ///
    /// Measured on the Turkish WebFAQ retrieval set (144 846 documents,
    /// 10 000 queries) through the engine: `prefix=6` moves nDCG@10 from
    /// 0.4841 to 0.5547, +14.6%, and closes about two fifths of the distance
    /// to a 278M-parameter multilingual transformer (0.650) with no model at
    /// all. It is not free -- the index goes 58 MB -> 182 MB and a query
    /// 82 -> 485 us, both roughly the 3.4x more postings it indexes.
    ///
    /// Character n-grams were measured against it and lost on cost: 4-grams
    /// score about the same for 6.0x the postings, and 3..5-grams cost 15.6x
    /// to score slightly *less*. On English SciFact the whole option is worth
    /// +0.0137, which is why it is off unless asked for -- the corpus decides
    /// this trade, not the engine.
    pub prefix_max: u8,
    /// Shortest prefix indexed. Below 3 the terms stop discriminating.
    pub prefix_min: u8,
}

impl Default for TextIndexSpec {
    fn default() -> Self {
        TextIndexSpec {
            k1_pct: 90,
            b_pct: 40,
            prefix_max: 0,
            prefix_min: 3,
        }
    }
}

impl TextIndexSpec {
    pub fn k1(&self) -> f32 {
        self.k1_pct as f32 / 100.0
    }
    pub fn b(&self) -> f32 {
        self.b_pct as f32 / 100.0
    }
    /// The prefix lengths to index, `None` when the option is off.
    pub fn prefixes(&self) -> Option<std::ops::RangeInclusive<usize>> {
        if self.prefix_max == 0 || self.prefix_max < self.prefix_min {
            return None;
        }
        Some(self.prefix_min as usize..=self.prefix_max as usize)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexKind {
    None,
    /// Hash index for equality lookups.
    Hash,
    /// Approximate nearest neighbour index (HNSW).
    Vector(VectorIndexSpec),
    /// Inverted index with BM25 scoring, behind `match`.
    Text(TextIndexSpec),
    /// Ordered index: ranges, equality, and `order ... limit` walked in
    /// order (see [`crate::sorted`]).
    Sorted,
    /// Inverted index over a sparse vector's dimensions, behind `near` on a
    /// `sparse<N>` field (see [`crate::sparse`]).
    Inverted,
}

impl IndexKind {
    /// Whether this index can be built over `field`, of type `ty`: the one
    /// rule `create collection`, `create index` and `fenec import --index`
    /// all go through. `quant=bit` was refused with l2 and dot only where a
    /// collection was created, and a `create index` taking it silently
    /// answered `near` from candidates chosen by sign alone.
    pub fn check(&self, field: &str, ty: &DataType) -> Result<()> {
        match self {
            IndexKind::Vector(spec) => {
                if !matches!(ty, DataType::Vector(..)) {
                    return Err(Error::Type(format!(
                        "field `{field}` is not vector<N>, no vector index can be built"
                    )));
                }
                if spec.quant == Quant::Bit && spec.metric != Metric::Cosine {
                    return Err(Error::Query(format!(
                        "`{field}`: quant=bit keeps the signs of unit vectors, so it needs \
                         the cosine metric, not {}",
                        spec.metric.name()
                    )));
                }
            }
            IndexKind::Text(_) if !matches!(ty, DataType::Text) => {
                return Err(Error::Type(format!(
                    "field `{field}` is not text, no full-text index can be built"
                )));
            }
            IndexKind::Sorted if !crate::sorted::SortedIndex::supports(ty) => {
                return Err(Error::Type(format!(
                    "field `{field}` is not int, float, timestamp or text, no ordered index \
                     can be built"
                )));
            }
            IndexKind::Inverted if !matches!(ty, DataType::Sparse(_)) => {
                return Err(Error::Type(format!(
                    "field `{field}` is not sparse<N>, no inverted index can be built \
                     (a text field's is @text)"
                )));
            }
            _ => {}
        }
        if let DataType::Sparse(d) = ty {
            if *d == 0 || *d > crate::sparse::MAX_DIM {
                return Err(Error::Type(format!(
                    "`{field}`: a sparse vector's dimension is 1 to {}",
                    crate::sparse::MAX_DIM
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: DataType,
    pub index: IndexKind,
    pub required: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, ty: DataType) -> Field {
        Field {
            name: name.into(),
            ty,
            index: IndexKind::None,
            required: false,
        }
    }
    pub fn indexed(mut self, kind: IndexKind) -> Field {
        self.index = kind;
        self
    }
    pub fn required(mut self) -> Field {
        self.required = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(name: impl Into<String>, fields: Vec<Field>) -> Result<Schema> {
        let name = name.into();
        let mut seen = Vec::new();
        for f in &fields {
            if f.name == "id" {
                return Err(Error::Query(
                    "`id` is a reserved field, it cannot be declared in a schema".into(),
                ));
            }
            if seen.contains(&f.name) {
                return Err(Error::Exists(format!("duplicate field `{}`", f.name)));
            }
            f.index.check(&f.name, &f.ty)?;
            seen.push(f.name.clone());
        }
        Ok(Schema { name, fields })
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn field_pos(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    /// Fields carrying a full-text index.
    pub fn text_fields(&self) -> Vec<(usize, &Field, TextIndexSpec)> {
        self.fields
            .iter()
            .enumerate()
            .filter_map(|(i, f)| match &f.index {
                IndexKind::Text(spec) => Some((i, f, *spec)),
                _ => None,
            })
            .collect()
    }

    /// Fields carrying a vector index (name, dimension, spec).
    pub fn vector_fields(&self) -> Vec<(usize, &Field, VectorIndexSpec)> {
        self.fields
            .iter()
            .enumerate()
            .filter_map(|(i, f)| match &f.index {
                IndexKind::Vector(spec) => Some((i, f, *spec)),
                _ => None,
            })
            .collect()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        encode_str(&mut out, &self.name);
        put_uvarint(&mut out, self.fields.len() as u64);
        for f in &self.fields {
            encode_str(&mut out, &f.name);
            encode_type(&mut out, &f.ty);
            out.push(f.required as u8);
            match &f.index {
                IndexKind::None => out.push(0),
                IndexKind::Hash => out.push(1),
                IndexKind::Vector(spec) => {
                    // A quantized index is a kind of its own, so that a version
                    // that knows no quantization refuses the file rather than
                    // reading it as full vectors; every other index is written
                    // as it always was.
                    out.push(if spec.quant == Quant::None { 2 } else { 5 });
                    out.push(spec.metric.code());
                    put_uvarint(&mut out, spec.m as u64);
                    put_uvarint(&mut out, spec.ef_construction as u64);
                    put_uvarint(&mut out, spec.ef_search as u64);
                    if spec.quant != Quant::None {
                        out.push(spec.quant.code());
                    }
                }
                IndexKind::Text(spec) => {
                    out.push(3);
                    put_uvarint(&mut out, spec.k1_pct as u64);
                    put_uvarint(&mut out, spec.b_pct as u64);
                    put_uvarint(&mut out, spec.prefix_max as u64);
                    put_uvarint(&mut out, spec.prefix_min as u64);
                }
                IndexKind::Sorted => out.push(4),
                IndexKind::Inverted => out.push(6),
            }
        }
        out
    }

    pub fn decode(buf: &[u8], pos: &mut usize) -> Result<Schema> {
        let name = decode_str(buf, pos)?;
        let n = get_uvarint(buf, pos)? as usize;
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            let fname = decode_str(buf, pos)?;
            let ty = decode_type(buf, pos)?;
            let required = buf[*pos] != 0;
            *pos += 1;
            let kind = buf[*pos];
            *pos += 1;
            let index = match kind {
                0 => IndexKind::None,
                1 => IndexKind::Hash,
                2 | 5 => {
                    let metric = Metric::from_code(buf[*pos])?;
                    *pos += 1;
                    let mut spec = VectorIndexSpec {
                        metric,
                        m: get_uvarint(buf, pos)? as usize,
                        ef_construction: get_uvarint(buf, pos)? as usize,
                        ef_search: get_uvarint(buf, pos)? as usize,
                        quant: Quant::None,
                    };
                    if kind == 5 {
                        let c = *buf
                            .get(*pos)
                            .ok_or_else(|| Error::Corrupt("schema ended early".into()))?;
                        *pos += 1;
                        spec.quant = Quant::from_code(c)
                            .ok_or_else(|| Error::Corrupt(format!("unknown quantization {c}")))?;
                    }
                    IndexKind::Vector(spec)
                }
                3 => IndexKind::Text(TextIndexSpec {
                    k1_pct: get_uvarint(buf, pos)? as u16,
                    b_pct: get_uvarint(buf, pos)? as u16,
                    prefix_max: get_uvarint(buf, pos)? as u8,
                    prefix_min: get_uvarint(buf, pos)? as u8,
                }),
                4 => IndexKind::Sorted,
                6 => IndexKind::Inverted,
                o => return Err(Error::Corrupt(format!("unknown index kind {o}"))),
            };
            fields.push(Field {
                name: fname,
                ty,
                index,
                required,
            });
        }
        Ok(Schema { name, fields })
    }
}
