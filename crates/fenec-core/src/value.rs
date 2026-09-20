use crate::error::{Error, Result};
use std::fmt;

/// Document id. Monotonically increasing, 64-bit.
pub type DocId = u64;

/// Storage precision of vector elements.
///
/// The runtime representation is `Value::Vector(Vec<f32>)` either way;
/// `F16` only halves the on-disk record and the HNSW arena. The values
/// embedding models produce fit comfortably into the f16 range
/// (+-65504, ~3 decimal digits), so the recall loss stays below the
/// measurable threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VecPrec {
    #[default]
    F32,
    F16,
}

/// fenecdb type system. Vectors are first-class citizens: `vector<768>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    Bool,
    Int,
    Float,
    Text,
    Bytes,
    /// UTC epoch milliseconds. Stored variable-length like `int`, but under
    /// a separate tag: display, the PostgreSQL type OID and `now()` all
    /// require that distinction -- as an alias over `int` every client
    /// would just see a raw number.
    Timestamp,
    /// Fixed-size vector. The dimension is known at schema time, which lets
    /// storage keep it inline without a length prefix (arena).
    Vector(usize, VecPrec),
    /// Homogeneous list (of scalar types).
    List(Box<DataType>),
}

impl DataType {
    pub fn name(&self) -> String {
        match self {
            DataType::Bool => "bool".into(),
            DataType::Int => "int".into(),
            DataType::Float => "float".into(),
            DataType::Text => "text".into(),
            DataType::Bytes => "bytes".into(),
            DataType::Timestamp => "timestamp".into(),
            DataType::Vector(d, VecPrec::F32) => format!("vector<{d}>"),
            DataType::Vector(d, VecPrec::F16) => format!("vector<{d}, f16>"),
            DataType::List(inner) => format!("[{}]", inner.name()),
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// Runtime value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    /// UTC epoch milliseconds.
    Timestamp(i64),
    Vector(Vec<f32>),
    List(Vec<Value>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Text(_) => "text",
            Value::Bytes(_) => "bytes",
            Value::Timestamp(_) => "timestamp",
            Value::Vector(_) => "vector",
            Value::List(_) => "list",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Timestamp(ms) => Some(*ms as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_vector(&self) -> Option<&[f32]> {
        match self {
            Value::Vector(v) => Some(v),
            _ => None,
        }
    }

    /// Coerces the value to the target type (numeric widening and
    /// list->vector conversion included). Schema validation funnels
    /// through this single point.
    pub fn coerce(self, ty: &DataType) -> Result<Value> {
        if self.is_null() {
            return Ok(Value::Null);
        }
        match (ty, self) {
            (DataType::Bool, Value::Bool(b)) => Ok(Value::Bool(b)),
            (DataType::Int, Value::Int(i)) => Ok(Value::Int(i)),
            (DataType::Int, Value::Float(f)) if f.fract() == 0.0 => Ok(Value::Int(f as i64)),
            (DataType::Float, Value::Float(f)) => Ok(Value::Float(f)),
            (DataType::Float, Value::Int(i)) => Ok(Value::Float(i as f64)),
            (DataType::Text, Value::Text(s)) => Ok(Value::Text(s)),
            (DataType::Bytes, Value::Bytes(b)) => Ok(Value::Bytes(b)),
            (DataType::Bytes, Value::Text(s)) => Ok(Value::Bytes(s.into_bytes())),
            (DataType::Timestamp, Value::Timestamp(ms)) => Ok(Value::Timestamp(ms)),
            // Epoch milliseconds directly; arithmetic results such as
            // `now() - 86400000` also pass through here.
            (DataType::Timestamp, Value::Int(ms)) => Ok(Value::Timestamp(ms)),
            (DataType::Timestamp, Value::Text(s)) => {
                Ok(Value::Timestamp(crate::time::parse(&s)?))
            }
            (DataType::Int, Value::Timestamp(ms)) => Ok(Value::Int(ms)),
            (DataType::Text, Value::Timestamp(ms)) => {
                Ok(Value::Text(crate::time::format_iso(ms)))
            }
            (DataType::Vector(dim, _), Value::Vector(v)) => check_dim(v, *dim),
            (DataType::Vector(dim, _), Value::List(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it.as_f64() {
                        Some(f) => out.push(f as f32),
                        None => {
                            return Err(Error::Type(format!(
                                "vector element must be numeric, found {}",
                                it.type_name()
                            )))
                        }
                    }
                }
                check_dim(out, *dim)
            }
            // On the JSON side an all-numeric array is parsed as a vector
            // (json.rs), because `near $1` is evaluated without schema
            // context. If the target field is a list it is converted back
            // here; without this arm `[int]`/`[float]` fields could not be
            // filled from the browser and fenec-pg paths. Values drop to f32
            // at that stage, so `[float]` loses precision one way; `[int]`
            // stays lossless for whole numbers.
            (DataType::List(inner), Value::Vector(v)) => {
                let mut out = Vec::with_capacity(v.len());
                for f in v {
                    out.push(Value::Float(f as f64).coerce(inner)?);
                }
                Ok(Value::List(out))
            }
            (DataType::List(inner), Value::List(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(it.coerce(inner)?);
                }
                Ok(Value::List(out))
            }
            (t, v) => Err(Error::Type(format!(
                "expected {}, found {}",
                t.name(),
                v.type_name()
            ))),
        }
    }

    /// Backstop of the total ordering for types that cannot be compared.
    /// Numeric types form a single group (Int <-> Float convert), and so do
    /// Text and Bytes -- since the schema coerces a `text` value into a
    /// `bytes` field, the two are compared within the same group.
    fn type_rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Float(_) | Value::Timestamp(_) => 2,
            Value::Text(_) | Value::Bytes(_) => 3,
            Value::Vector(_) => 4,
            Value::List(_) => 5,
        }
    }

    /// Total ordering for sorting/comparison. Null counts as the smallest.
    pub fn cmp_value(&self, other: &Value) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            // A timestamp must be comparable with a text literal: the write
            // path parses the text, so `where created_at >= "2026-01-01"`
            // must agree. Unparseable text falls back to type rank.
            (Value::Timestamp(a), Value::Text(b)) => match crate::time::parse(b) {
                Ok(ms) => a.cmp(&ms),
                Err(_) => Ordering::Less,
            },
            (Value::Text(a), Value::Timestamp(b)) => match crate::time::parse(a) {
                Ok(ms) => ms.cmp(b),
                Err(_) => Ordering::Greater,
            },
            // A `bytes` field must be comparable with a text literal: the
            // write path turns Text into Bytes, so the read path must agree.
            (Value::Text(a), Value::Bytes(b)) => a.as_bytes().cmp(&b[..]),
            (Value::Bytes(a), Value::Text(b)) => a[..].cmp(b.as_bytes()),
            // Arrays element by element: previously neither side was
            // numeric, so this returned Equal -- `tags = ["rust"]` matched
            // every row.
            (Value::List(a), Value::List(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let o = x.cmp_value(y);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Value::Vector(a), Value::Vector(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let o = x.partial_cmp(y).unwrap_or(Ordering::Equal);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a.len().cmp(&b.len())
            }
            _ => match (self.as_f64(), other.as_f64()) {
                (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
                // Incomparable types fall back to type rank. This used to
                // return Equal: `year = "abc"` matched every row.
                _ => self.type_rank().cmp(&other.type_rank()),
            },
        }
    }
}

fn check_dim(v: Vec<f32>, dim: usize) -> Result<Value> {
    if v.len() != dim {
        return Err(Error::Type(format!(
            "vector dimension must be {}, got {}",
            dim,
            v.len()
        )));
    }
    Ok(Value::Vector(v))
}

/// Field name -> value. Field order comes from the schema, so a Vec suffices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    pub id: DocId,
    pub fields: Vec<(String, Value)>,
}

impl Document {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    pub fn set(&mut self, name: &str, value: Value) {
        if let Some(slot) = self.fields.iter_mut().find(|(k, _)| k == name) {
            slot.1 = value;
        } else {
            self.fields.push((name.to_string(), value));
        }
    }
}
