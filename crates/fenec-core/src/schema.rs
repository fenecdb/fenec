use crate::codec::*;
use crate::collate::Collation;
use crate::error::{Error, Result};
use crate::value::{DataType, MAX_PATH_KEYS};

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
    /// A bit a component and nine bytes a vector, about a thirtieth of
    /// `f32`: the signs of the unit vector's distance from the nearest of
    /// the centres the index learns, so cosine only.
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

/// `ef_search` for a `quant=bit` index that names none. A bit code bounds
/// nothing, so the whole beam is read, and it estimates less closely than
/// an int8 code: over a million clustered 768-dimension vectors a beam of
/// 100 held 97.9% of the true ten and one of 200 99.8%, where int8 codes
/// held 97.1% at 100 and full vectors 97.4%; spread in every dimension, 100
/// held 89.5% and 200 97.6%, full vectors 97.8% at 100 (`make
/// quant-bench`). It was 400 while the codes were the vectors' own signs,
/// which a beam of 400 held 98.4% of the million's ten with, in 2.16 ms
/// where 200 takes 1.21 now.
pub const BIT_EF_SEARCH: usize = 200;

/// `ef_search` for every other index that names none. recall@10 measured at
/// 100 is 100%; at 64 it is 99%. ANN latency rises from 0.10 -> 0.13 ms,
/// which is still ~7x faster than the engines we compare against. The
/// accuracy is worth the trade.
pub const DEFAULT_EF_SEARCH: usize = 100;

/// HNSW parameters. The defaults were picked targeting ~95% recall /
/// a few hundred microseconds on 1M-scale collections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorIndexSpec {
    pub metric: Metric,
    /// Number of connections per layer.
    pub m: usize,
    /// Candidate list width during construction.
    pub ef_construction: usize,
    /// Default candidate list width at query time. 0 -- what `Default`
    /// gives -- until a schema or an index takes the spec, which settles it
    /// by the codes ([`VectorIndexSpec::resolved`]).
    pub ef_search: usize,
    /// What the index holds of each vector.
    pub quant: Quant,
}

impl VectorIndexSpec {
    /// The spec with `ef_search` settled: one left at 0 takes the beam its
    /// codes want, [`BIT_EF_SEARCH`] over bit codes and
    /// [`DEFAULT_EF_SEARCH`] otherwise. Only the parser used to know the bit
    /// beam, so an index built through the Rust API with `quant: Bit`
    /// searched a beam of 100: 82.5% of the true ten over a million
    /// vectors, where 400 held 98.4%, over the signs bit codes were then.
    /// Settled wherever a spec comes in -- a schema, a `create index`, a
    /// graph -- the file holds the number.
    pub fn resolved(mut self) -> VectorIndexSpec {
        if self.ef_search == 0 {
            self.ef_search = match self.quant {
                Quant::Bit => BIT_EF_SEARCH,
                _ => DEFAULT_EF_SEARCH,
            };
        }
        self
    }

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
            // Settled by the codes when a schema or an index takes it.
            ef_search: 0,
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
    /// Whether each character of a script written without spaces -- Han,
    /// kana, Hangul, Thai -- is indexed as well as each pair of them (`text`).
    ///
    /// Pairs find a word written in a longer run, and a query of one
    /// character finds only a run of one. On C-MTEB's EcomRetrieval, whose
    /// documents are product titles, characters as well move nDCG@10 0.439
    /// -> 0.528; on its CovidRetrieval, news, 0.868 -> 0.872, and on the
    /// Japanese JaGovFaqs 0.582 -> 0.576. For 1.6 to 1.9x the postings and a
    /// query 3 to 8x slower, so it is off unless asked for.
    pub chars: bool,
}

impl Default for TextIndexSpec {
    fn default() -> Self {
        TextIndexSpec {
            k1_pct: 90,
            b_pct: 40,
            prefix_max: 0,
            prefix_min: 3,
            chars: false,
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
    /// The options as `@text(...)` spells them: what every listing of a
    /// schema and every statement generated from one prints, so that an
    /// index made again from it is the same index.
    pub fn args(&self) -> String {
        let mut out = String::from("k1=");
        crate::num::f32_into(&mut out, self.k1());
        out.push_str(", b=");
        crate::num::f32_into(&mut out, self.b());
        if self.prefix_max != 0 {
            out.push_str(&format!(", prefix={}", self.prefix_max));
            if self.prefix_min != 3 {
                out.push_str(&format!(", prefix_min={}", self.prefix_min));
            }
        }
        if self.chars {
            out.push_str(", chars");
        }
        out
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
    /// Hash index for equality lookups. `unique` (`@unique`) refuses a
    /// second document holding a value one already holds, `null` aside: an
    /// index that only finds documents could be told nothing new, so the
    /// write path asks the bucket it would file the value under
    /// (`Database::unique_clash`).
    Hash {
        unique: bool,
    },
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
    /// `@hash`.
    pub const HASH: IndexKind = IndexKind::Hash { unique: false };
    /// `@unique`.
    pub const UNIQUE: IndexKind = IndexKind::Hash { unique: true };

    /// Whether it refuses a second document holding a value (`@unique`).
    pub fn is_unique(&self) -> bool {
        matches!(self, IndexKind::Hash { unique: true })
    }

    /// The kind with a vector index's `ef_search` settled
    /// ([`VectorIndexSpec::resolved`]).
    pub fn resolved(&self) -> IndexKind {
        match self {
            IndexKind::Vector(spec) => IndexKind::Vector(spec.resolved()),
            other => other.clone(),
        }
    }

    /// Whether this index can be built over `field`, of type `ty`: the one
    /// rule `create collection`, `create index` and `fenec import --index`
    /// all go through. `quant=bit` was refused with l2 and dot only where a
    /// collection was created, and a `create index` taking it silently
    /// answered `near` from candidates chosen by sign alone.
    pub fn check(&self, field: &str, ty: &DataType) -> Result<()> {
        // A path reads a value of no declared type: equality and order are
        // what it takes, and the text, vector and sparse indexes stay on
        // the fields that declare their type.
        if field.contains('.') && !matches!(self, IndexKind::Hash { .. } | IndexKind::Sorted) {
            return Err(Error::Query(format!(
                "`{field}` is a path: it takes @hash, @unique or @sorted"
            )));
        }
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
            IndexKind::Sorted if !crate::sorted::orderable(ty) => {
                return Err(Error::Type(format!(
                    "field `{field}` is not int, float, timestamp, text or json, no ordered \
                     index can be built"
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
    /// `collate tr`: the order its text compares in, wherever it compares
    /// -- `order`, `<` and `>`, `min` and `max`, a `@sorted` index -- rather
    /// than the order of its bytes. Equality stays the bytes', since the
    /// collation tells every two different strings apart.
    pub collate: Option<Collation>,
}

impl Field {
    pub fn new(name: impl Into<String>, ty: DataType) -> Field {
        Field {
            name: name.into(),
            ty,
            index: IndexKind::None,
            required: false,
            collate: None,
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
    pub fn collated(mut self, c: Collation) -> Field {
        self.collate = Some(c);
        self
    }
}

/// Text, or a list of it -- which compares element by element: what a
/// collation orders.
pub fn collatable(t: &DataType) -> bool {
    match t {
        DataType::Text => true,
        DataType::List(t) => **t == DataType::Text,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    /// The fields a document holds, in the order it holds them -- the
    /// positions every reader of a field passes the store.
    pub fields: Vec<Field>,
    /// The indexes on paths into `json` fields (`create index on docs
    /// (meta.lang) @hash`), each a field of type `json` named by its whole
    /// path, in the order of the fields they read into. Beside the fields
    /// rather than among them: a document holds no value for one, so a
    /// position in `fields` stays a place in the payload. The schema writes
    /// each with the field it reads into ([`Schema::encode`]).
    pub paths: Vec<Field>,
    /// Where a dropped field's value still is in the documents written
    /// before its drop, ascending: a document is a run of values in field
    /// order, so `alter ... drop field` leaves the place, skipped on read
    /// and written as `null`, rather than rewrite every document under the
    /// write lock. `compact` writes the documents without them and empties
    /// this. Every field after one sits a place further in the documents
    /// than in `fields` ([`Schema::place`]).
    pub dropped: Vec<usize>,
}

/// A name read as a path: the field and the keys past it (`meta` and
/// `source.rank` of `meta.source.rank`), `None` for a plain field's name.
/// A field's name holds no dot ([`Schema::new`]), so the first one ends it.
pub fn split_path(name: &str) -> Option<(&str, &str)> {
    name.split_once('.')
}

/// Where the field at `pos` is in a document, past the `dropped` places
/// before it (ascending): what the store and the schema both work out.
#[inline]
pub fn place(dropped: &[usize], pos: usize) -> usize {
    let mut at = pos;
    for &d in dropped {
        if d > at {
            break;
        }
        at += 1;
    }
    at
}

impl Schema {
    pub fn new(name: impl Into<String>, mut fields: Vec<Field>) -> Result<Schema> {
        let name = name.into();
        let mut seen = Vec::new();
        for f in &mut fields {
            f.index = f.index.resolved();
            if f.name == "id" {
                return Err(Error::Query(
                    "`id` is a reserved field, it cannot be declared in a schema".into(),
                ));
            }
            // A dot is where a path starts (`meta.lang`): a field named
            // with one could not be told from a path into another.
            if f.name.contains('.') || f.name.is_empty() {
                return Err(Error::Query(format!(
                    "`{}` cannot name a field: a name holds no dot, which starts a path",
                    f.name
                )));
            }
            if matches!(&f.ty, DataType::List(t) if **t == DataType::Json) {
                return Err(Error::Type(format!(
                    "`{}`: a list of json is a json field holding a list",
                    f.name
                )));
            }
            if seen.contains(&f.name) {
                return Err(Error::Exists(format!("duplicate field `{}`", f.name)));
            }
            if let Some(c) = f.collate.filter(|_| !collatable(&f.ty)) {
                return Err(Error::Query(format!(
                    "`collate {}` orders text; `{}` is {}",
                    c.name(),
                    f.name,
                    f.ty.name()
                )));
            }
            f.index.check(&f.name, &f.ty)?;
            seen.push(f.name.clone());
        }
        Ok(Schema {
            name,
            fields,
            paths: Vec::new(),
            dropped: Vec::new(),
        })
    }

    /// The field a path reads into, by its position, and the keys past it:
    /// `None` for a name with no dot. Refused where the field is not there
    /// or not `json`, where a key is empty, and past [`MAX_PATH_KEYS`] keys,
    /// which no value nests deep enough to answer.
    pub fn path_of<'a>(&self, name: &'a str) -> Result<Option<(usize, &'a str)>> {
        let Some((field, keys)) = split_path(name) else {
            return Ok(None);
        };
        let pos = self
            .field_pos(field)
            .ok_or_else(|| Error::NotFound(format!("field `{field}`")))?;
        if self.fields[pos].ty != DataType::Json {
            return Err(Error::Type(format!(
                "`{name}` is a path, and `{field}` is {}: a path reads into a json field",
                self.fields[pos].ty.name()
            )));
        }
        let mut n = 0;
        for k in keys.split('.') {
            if k.is_empty() {
                return Err(Error::Query(format!("`{name}`: a path's keys are names")));
            }
            n += 1;
        }
        if n > MAX_PATH_KEYS {
            return Err(Error::Query(format!(
                "`{name}`: a path names at most {MAX_PATH_KEYS} keys past its field"
            )));
        }
        Ok(Some((pos, keys)))
    }

    /// The index on a path, by the path's whole name.
    pub fn path(&self, name: &str) -> Option<&Field> {
        self.paths.iter().find(|f| f.name == name)
    }

    /// A field, or an index on a path, by name: what carries an index of
    /// that name.
    pub fn indexed(&self, name: &str) -> Option<&Field> {
        match split_path(name) {
            None => self.field(name),
            Some(_) => self.path(name),
        }
    }

    /// Puts an index on a path among the others, after those of the fields
    /// before its own and of its own: the order [`Schema::decode`] gives
    /// them back in, which keeps a schema read from the file equal to the
    /// one written.
    pub fn add_path(&mut self, f: Field) {
        let field = |n: &str| split_path(n).and_then(|(h, _)| self.field_pos(h));
        let mine = field(&f.name);
        let at = self
            .paths
            .iter()
            .position(|p| field(&p.name) > mine)
            .unwrap_or(self.paths.len());
        self.paths.insert(at, f);
    }

    /// Where the field at `pos` of [`Self::fields`] is in a document: past
    /// every dropped place before it.
    pub fn place(&self, pos: usize) -> usize {
        place(&self.dropped, pos)
    }

    /// The places a document holds: its fields and the dropped ones.
    pub fn width(&self) -> usize {
        self.fields.len() + self.dropped.len()
    }

    /// The values of a document's payload as its fields', a dropped place
    /// passed over and a field the payload ends before -- one added after
    /// it was written -- `null`.
    pub fn read_doc(&self, id: crate::value::DocId, buf: &[u8]) -> Result<crate::value::Document> {
        let mut fields = Vec::with_capacity(self.fields.len());
        let (mut pos, mut at) = (0, 0);
        for (i, f) in self.fields.iter().enumerate() {
            let want = self.place(i);
            while at < want {
                skip_field(buf, &mut pos)?;
                at += 1;
            }
            let v = match pos < buf.len() {
                true => decode_value(buf, &mut pos)?,
                false => crate::value::Value::Null,
            };
            at += 1;
            fields.push((f.name.clone(), v));
        }
        Ok(crate::value::Document { id, fields })
    }

    /// A payload without its dropped places: what `compact` writes. Each
    /// value kept is copied as its bytes stand.
    pub fn without_dropped(&self, buf: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(buf.len());
        let (mut pos, mut at) = (0, 0);
        while pos < buf.len() {
            let from = pos;
            skip_value(buf, &mut pos)?;
            if !self.dropped.contains(&at) {
                out.extend_from_slice(&buf[from..pos]);
            }
            at += 1;
        }
        Ok(out)
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
        put_uvarint(&mut out, self.width() as u64);
        let mut fields = self.fields.iter();
        for at in 0..self.width() {
            // A dropped place where it stands, nameless, behind a type tag
            // of its own: a version that knows no drop refuses the file
            // rather than read every field after it one place early.
            if self.dropped.contains(&at) {
                encode_str(&mut out, "");
                out.push(TAG_DROPPED);
                continue;
            }
            let Some(f) = fields.next() else { break };
            encode_str(&mut out, &f.name);
            if let Some(c) = f.collate {
                out.push(TAG_COLLATED);
                out.push(c.code());
            }
            encode_type(&mut out, &f.ty);
            out.push(f.required as u8);
            encode_index(&mut out, &f.index);
            // The indexes on paths into a json field go with it, each its
            // keys and its kind. Inside the field rather than after the
            // fields: a version that knows no json refuses the field's type
            // before it reaches them, and none can read them as something
            // else.
            if f.ty == DataType::Json {
                let mine: Vec<&Field> = self
                    .paths
                    .iter()
                    .filter(|p| split_path(&p.name).is_some_and(|(h, _)| h == f.name))
                    .collect();
                put_uvarint(&mut out, mine.len() as u64);
                for p in mine {
                    let keys = split_path(&p.name).map_or("", |(_, k)| k);
                    encode_str(&mut out, keys);
                    encode_index(&mut out, &p.index);
                }
            }
        }
        out
    }

    pub fn decode(buf: &[u8], pos: &mut usize) -> Result<Schema> {
        let name = decode_str(buf, pos)?;
        let n = get_uvarint(buf, pos)? as usize;
        let mut fields = Vec::with_capacity(n.min(buf.len()));
        let mut paths = Vec::new();
        let mut dropped = Vec::new();
        for at in 0..n {
            let fname = decode_str(buf, pos)?;
            if buf.get(*pos) == Some(&TAG_DROPPED) {
                *pos += 1;
                dropped.push(at);
                continue;
            }
            let collate = match buf.get(*pos) {
                Some(&TAG_COLLATED) => {
                    let c = *buf
                        .get(*pos + 1)
                        .ok_or_else(|| Error::Corrupt("schema ended early".into()))?;
                    *pos += 2;
                    Some(
                        Collation::from_code(c)
                            .ok_or_else(|| Error::Corrupt(format!("unknown collation {c}")))?,
                    )
                }
                _ => None,
            };
            let ty = decode_type(buf, pos)?;
            let required = *buf
                .get(*pos)
                .ok_or_else(|| Error::Corrupt("schema ended early".into()))?
                != 0;
            *pos += 1;
            let index = decode_index(buf, pos)?;
            if ty == DataType::Json {
                let n = get_uvarint(buf, pos)? as usize;
                for _ in 0..n {
                    let keys = decode_str(buf, pos)?;
                    let index = decode_index(buf, pos)?;
                    paths
                        .push(Field::new(format!("{fname}.{keys}"), DataType::Json).indexed(index));
                }
            }
            fields.push(Field {
                name: fname,
                ty,
                index,
                required,
                collate,
            });
        }
        Ok(Schema {
            name,
            fields,
            paths,
            dropped,
        })
    }
}

/// An index's kind as a schema writes it, behind its field.
fn encode_index(out: &mut Vec<u8>, index: &IndexKind) {
    match index {
        IndexKind::None => out.push(0),
        // A unique one is a kind of its own, as a quantized graph is: a
        // version that knows no `@unique` refuses the file rather than open
        // it as a plain hash and take the duplicates it would have refused.
        IndexKind::Hash { unique } => out.push(if *unique { 8 } else { 1 }),
        IndexKind::Vector(spec) => {
            // A quantized index is a kind of its own, so that a version that
            // knows no quantization refuses the file rather than reading it
            // as full vectors; every other index is written as it always
            // was.
            out.push(if spec.quant == Quant::None { 2 } else { 5 });
            out.push(spec.metric.code());
            put_uvarint(out, spec.m as u64);
            put_uvarint(out, spec.ef_construction as u64);
            put_uvarint(out, spec.ef_search as u64);
            if spec.quant != Quant::None {
                out.push(spec.quant.code());
            }
        }
        IndexKind::Text(spec) => {
            // One that indexes characters is a kind of its own, as a
            // quantized index is: a version that knows no `chars` refuses
            // the file rather than index pairs alone and answer a query of
            // one character with nothing.
            out.push(if spec.chars { 7 } else { 3 });
            put_uvarint(out, spec.k1_pct as u64);
            put_uvarint(out, spec.b_pct as u64);
            put_uvarint(out, spec.prefix_max as u64);
            put_uvarint(out, spec.prefix_min as u64);
        }
        IndexKind::Sorted => out.push(4),
        IndexKind::Inverted => out.push(6),
    }
}

/// An index's kind as [`encode_index`] wrote it.
fn decode_index(buf: &[u8], pos: &mut usize) -> Result<IndexKind> {
    let ended = || Error::Corrupt("schema ended early".into());
    let kind = *buf.get(*pos).ok_or_else(ended)?;
    *pos += 1;
    Ok(match kind {
        0 => IndexKind::None,
        1 | 8 => IndexKind::Hash { unique: kind == 8 },
        2 | 5 => {
            let metric = Metric::from_code(*buf.get(*pos).ok_or_else(ended)?)?;
            *pos += 1;
            let mut spec = VectorIndexSpec {
                metric,
                m: get_uvarint(buf, pos)? as usize,
                ef_construction: get_uvarint(buf, pos)? as usize,
                ef_search: get_uvarint(buf, pos)? as usize,
                quant: Quant::None,
            };
            if kind == 5 {
                let c = *buf.get(*pos).ok_or_else(ended)?;
                *pos += 1;
                spec.quant = Quant::from_code(c)
                    .ok_or_else(|| Error::Corrupt(format!("unknown quantization {c}")))?;
            }
            IndexKind::Vector(spec.resolved())
        }
        3 | 7 => IndexKind::Text(TextIndexSpec {
            k1_pct: get_uvarint(buf, pos)? as u16,
            b_pct: get_uvarint(buf, pos)? as u16,
            prefix_max: get_uvarint(buf, pos)? as u8,
            prefix_min: get_uvarint(buf, pos)? as u8,
            chars: kind == 7,
        }),
        4 => IndexKind::Sorted,
        6 => IndexKind::Inverted,
        o => return Err(Error::Corrupt(format!("unknown index kind {o}"))),
    })
}
