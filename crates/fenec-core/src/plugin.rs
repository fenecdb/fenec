//! Plugin system -- inspired by PostgreSQL's extension model.
//!
//! A plugin can hook into:
//!   * scalar functions (`register_fn`)  -> callable in FenecQL expressions
//!   * write hooks      (`register_hook`) -> trigger-like
//!   * write hooks                        -> e.g. a JWT policy's row check
//!
//! The core knows nothing about plugins; fenec-http checks a scoped token's
//! writes through this interface, and fenec-server guards a mirrored
//! collection with it.

use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::value::{Document, Value};
use std::sync::Arc;

/// A scalar function callable from FenecQL expressions.
pub trait ScalarFn: Send + Sync {
    fn call(&self, args: &[Value]) -> Result<Value>;
    /// (min, max) argument count; a `None` max means variadic.
    fn arity(&self) -> (usize, Option<usize>) {
        (0, None)
    }
    fn doc(&self) -> &str {
        ""
    }
}

impl<F> ScalarFn for F
where
    F: Fn(&[Value]) -> Result<Value> + Send + Sync,
{
    fn call(&self, args: &[Value]) -> Result<Value> {
        self(args)
    }
}

/// Hook on the write path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    Insert,
    Update,
    Delete,
}

pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    /// Called before a write into the collection `schema` describes. The
    /// document may be modified (embedding generation, for instance).
    /// Returning `Err` aborts the write. The schema rather than the name:
    /// a filter evaluated over the document has to compare a field in its
    /// collation, as the same filter does when it reads.
    fn before_write(&self, _schema: &Schema, _op: WriteOp, _doc: &mut Document) -> Result<()> {
        Ok(())
    }
    /// Called after [`Self::before_write`] for a write over a document the
    /// collection holds -- a `set`'s row, a `put` naming its id -- with the
    /// document as it was: what the write changes is the difference.
    /// Returning `Err` aborts the write. A scoped token's grants of some
    /// fields alone are judged here (`fenec-http`'s `access`).
    fn before_overwrite(&self, _schema: &Schema, _old: &Document, _doc: &Document) -> Result<()> {
        Ok(())
    }
    fn after_write(&self, _collection: &str, _op: WriteOp, _doc: &Document) -> Result<()> {
        Ok(())
    }
    /// Called for each row a write answers (`returning`): a `set`'s as it
    /// wrote it, a `del`'s as it was. Returning `Err` refuses the
    /// statement, which is put back whole: a scoped token is answered the
    /// rows it may read alone, and one it wrote and may not read is no
    /// write of its, rather than one it is not told of (`fenec-http`'s
    /// `access`).
    fn answering(&self, _schema: &Schema, _doc: &Document) -> Result<()> {
        Ok(())
    }
}

/// A function as the registry holds it: its name, lowered, the function,
/// and its arity.
type Entry = (String, Arc<dyn ScalarFn>, (usize, Option<usize>));

/// Cloned, the functions and hooks are shared: what a pinned read
/// ([`crate::engine::Pinned`]) calls a row's functions through.
#[derive(Default, Clone)]
pub struct Registry {
    /// Name, lowered, -> function: a `Vec` searched by name rather than a
    /// map. Four are built in, a lookup through the map lowered the name
    /// into a new `String` on every call of every row, and the map's code
    /// was 0.9 KB of the browser module. Each beside its arity, asked once
    /// as it is registered: a builtin's `call` is a function pointer behind
    /// the vtable, and with `arity` asked a call as well, `lower()` over a
    /// scan took 158 -> 160 ns a row.
    functions: Vec<Entry>,
    hooks: Vec<Arc<dyn Hook>>,
    plugins: Vec<(String, String)>,
}

impl Registry {
    pub fn with_builtins() -> Registry {
        let mut r = Registry::default();
        builtins::install(&mut r);
        r
    }

    pub fn register_fn(&mut self, name: &str, f: Arc<dyn ScalarFn>) -> Result<()> {
        // The name as given against the lowered ones, as the map's
        // `contains_key` compared it: `Lower` replaces `lower`.
        // A point's two are the engine's own beside the registry's: a
        // filter binds them, and `@geo` answers them, as the builtins work
        // them out -- replaced, a row's answer and the index's would part.
        let taken = crate::geo::is_geo_call(name) && self.function(name).is_some();
        if taken || self.functions.iter().any(|(n, ..)| n == name) {
            return Err(Error::Exists(format!(
                "function `{name}` is already registered"
            )));
        }
        let lowered = name.to_ascii_lowercase();
        let arity = f.arity();
        match self.functions.iter_mut().find(|(n, ..)| *n == lowered) {
            Some(slot) => (slot.1, slot.2) = (f, arity),
            None => self.functions.push((lowered, f, arity)),
        }
        Ok(())
    }

    pub fn register_hook(&mut self, h: Arc<dyn Hook>) {
        self.hooks.push(h);
    }

    pub fn function(&self, name: &str) -> Option<&Arc<dyn ScalarFn>> {
        self.entry(name).map(|(_, f, _)| f)
    }

    fn entry(&self, name: &str) -> Option<&Entry> {
        self.functions
            .iter()
            .find(|(n, ..)| n.eq_ignore_ascii_case(name))
    }

    pub fn function_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.functions.iter().map(|(n, ..)| n.clone()).collect();
        v.sort();
        v
    }

    pub fn hooks(&self) -> &[Arc<dyn Hook>] {
        &self.hooks
    }

    pub fn plugins(&self) -> &[(String, String)] {
        &self.plugins
    }

    pub fn install(&mut self, p: &dyn Plugin) -> Result<()> {
        if self.plugins.iter().any(|(n, _)| n == p.name()) {
            return Err(Error::Exists(format!(
                "plugin `{}` is already installed",
                p.name()
            )));
        }
        p.init(self)?;
        self.plugins
            .push((p.name().to_string(), p.version().to_string()));
        Ok(())
    }

    pub fn call(&self, name: &str, args: &[Value]) -> Result<Value> {
        let (_, f, (lo, hi)) = self.entry(name).ok_or_else(|| {
            if !crate::FULL {
                if let Some(feature) = missing_function(name) {
                    return crate::error::not_built(&format!("`{name}`"), feature);
                }
            }
            match name.eq_ignore_ascii_case(crate::query::EXPIRED) {
                true => Error::Query(
                    "`expired()` reads the rows past their `@ttl`, and this collection \
                     has no `@ttl`"
                        .into(),
                ),
                false => Error::Query(format!("unknown function `{name}`")),
            }
        })?;
        let (lo, hi) = (*lo, *hi);
        if args.len() < lo || hi.map(|h| args.len() > h).unwrap_or(false) {
            return Err(Error::Query(format!(
                "function `{name}` expects {lo}{} argument(s), got {}",
                hi.map(|h| format!("-{h}")).unwrap_or("+".into()),
                args.len()
            )));
        }
        f.call(args)
    }
}

/// The feature a builtin function named `name` comes with, where this build
/// was made without it (`Cargo.toml`): its call is refused naming the
/// feature rather than as a function nobody wrote. A registry's own
/// function by that name is found before this is asked.
fn missing_function(name: &str) -> Option<&'static str> {
    const GEO: &[&str] = if cfg!(feature = "geo") {
        &[]
    } else {
        &[crate::geo::DISTANCE, crate::geo::WITHIN]
    };
    const ANALYTICS: &[&str] = if cfg!(feature = "analytics") {
        &[]
    } else {
        &["bucket", "hll_estimate"]
    };
    for (feature, names) in [("geo", GEO), ("analytics", ANALYTICS)] {
        if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Some(feature);
        }
    }
    None
}

pub trait Plugin {
    fn name(&self) -> &str;
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn init(&self, reg: &mut Registry) -> Result<()>;
}

// ------------------------------------------------------------- builtins

pub mod builtins {
    use super::*;
    use crate::vector;

    /// A builtin: its function, its arity and what it does. One type for
    /// all of them, in a table: a type of its own each, made by a macro,
    /// was 17 vtables and 17 copies of `arity` and `doc`, 0.9 KB of the
    /// browser module.
    #[derive(Clone, Copy)]
    struct Builtin {
        name: &'static str,
        lo: usize,
        hi: Option<usize>,
        doc: &'static str,
        f: fn(&[Value]) -> Result<Value>,
    }

    impl ScalarFn for Builtin {
        fn call(&self, args: &[Value]) -> Result<Value> {
            (self.f)(args)
        }
        fn arity(&self) -> (usize, Option<usize>) {
            (self.lo, self.hi)
        }
        fn doc(&self) -> &str {
            self.doc
        }
    }

    const fn b(
        name: &'static str,
        lo: usize,
        hi: Option<usize>,
        doc: &'static str,
        f: fn(&[Value]) -> Result<Value>,
    ) -> Builtin {
        Builtin {
            name,
            lo,
            hi,
            doc,
            f,
        }
    }

    fn vec_arg(v: &Value) -> Result<Vec<f32>> {
        match v {
            Value::Vector(x) => Ok(x.clone()),
            Value::List(items) => Value::floats(items)
                .map_err(|_| Error::Type("vector element must be numeric".into())),
            other => Err(Error::Type(format!(
                "expected a vector, found {}",
                other.type_name()
            ))),
        }
    }

    // As PostgreSQL's: nulls are passed over, and null only when every
    // value is. One function for both, the order its one difference.
    fn extreme(a: &[Value], larger: bool) -> Value {
        let mut best: Option<&Value> = None;
        for v in a.iter().filter(|v| !v.is_null()) {
            let o = best.map(|b| v.cmp_value(b));
            let want = if larger {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
            if o.is_none_or(|o| o == want) {
                best = Some(v);
            }
        }
        best.cloned().unwrap_or(Value::Null)
    }

    /// Two vectors of one length, as the functions over a pair take them.
    fn pair(a: &[Value]) -> Result<(Vec<f32>, Vec<f32>)> {
        let (x, y) = (vec_arg(&a[0])?, vec_arg(&a[1])?);
        if x.len() != y.len() {
            return Err(Error::Type("vector dimensions do not match".into()));
        }
        Ok((x, y))
    }

    static BUILTINS: &[Builtin] = &[
        b("lower", 1, Some(1), "lowercases text", |a| {
            Ok(match &a[0] {
                // Unicode, not ASCII: see the note on `query::like_match`
                // for what the cheaper mapping was measured to cost.
                Value::Text(s) => Value::Text(crate::case::lower(s)),
                v => v.clone(),
            })
        }),
        b("upper", 1, Some(1), "uppercases text", |a| {
            Ok(match &a[0] {
                Value::Text(s) => Value::Text(crate::case::upper(s)),
                v => v.clone(),
            })
        }),
        b("len", 1, Some(1), "length of text/list/vector", |a| {
            Ok(Value::Int(match &a[0] {
                Value::Text(s) => s.chars().count() as i64,
                Value::Bytes(b) => b.len() as i64,
                Value::List(l) => l.len() as i64,
                Value::Vector(v) => v.len() as i64,
                _ => 0,
            }))
        }),
        b(
            "coalesce",
            1,
            None,
            "returns the first non-null value",
            |a| {
                Ok(a.iter()
                    .find(|v| !v.is_null())
                    .cloned()
                    .unwrap_or(Value::Null))
            },
        ),
        b(
            "cosine",
            2,
            Some(2),
            "cosine similarity between two vectors",
            |a| {
                let (x, y) = pair(a)?;
                let (nx, ny) = (vector::normalized(&x), vector::normalized(&y));
                Ok(Value::Float(vector::dot(&nx, &ny) as f64))
            },
        ),
        b(
            "l2",
            2,
            Some(2),
            "euclidean distance between two vectors",
            |a| {
                let (x, y) = pair(a)?;
                Ok(Value::Float(vector::l2_sq(&x, &y).sqrt() as f64))
            },
        ),
        b("dot", 2, Some(2), "inner product", |a| {
            let (x, y) = pair(a)?;
            Ok(Value::Float(vector::dot(&x, &y) as f64))
        }),
        b("now", 0, Some(0), "current time (UTC)", |_a| {
            Ok(Value::Timestamp(crate::time::now_ms()?))
        }),
        b(
            "timestamp",
            1,
            Some(1),
            "converts text or epoch milliseconds into a timestamp",
            |a| a[0].clone().coerce(&crate::value::DataType::Timestamp),
        ),
        #[cfg(feature = "analytics")]
        b(
            "bucket",
            2,
            Some(2),
            "the start of the interval a timestamp falls in: bucket(at, 15m)",
            |a| crate::query::Interval::of(&a[1])?.truncate(&a[0]),
        ),
        b("greatest", 1, None, "the largest of its values", |a| {
            Ok(extreme(a, true))
        }),
        b("least", 1, None, "the smallest of its values", |a| {
            Ok(extreme(a, false))
        }),
        #[cfg(feature = "analytics")]
        b(
            "hll_estimate",
            1,
            Some(1),
            "the distinct count a sketch from hll_accumulate or hll_combine holds",
            |a| {
                Ok(match &a[0] {
                    Value::Null => Value::Null,
                    Value::Bytes(b) => {
                        Value::Int(crate::hll::Sketch::from_bytes(b)?.estimate() as i64)
                    }
                    other => {
                        return Err(Error::Type(format!(
                            "`hll_estimate` takes a sketch, the bytes `hll_accumulate` makes; \
                             found {}",
                            other.type_name()
                        )))
                    }
                })
            },
        ),
        #[cfg(feature = "geo")]
        b(
            "distance",
            2,
            Some(2),
            "the metres between two points [lon, lat], as Redis's GEODIST",
            crate::geo::distance_fn,
        ),
        #[cfg(feature = "geo")]
        b(
            "within",
            2,
            Some(2),
            "whether a point lies in a box [west, south, east, north]",
            crate::geo::within_fn,
        ),
        b("norm", 1, Some(1), "vector length (L2 norm)", |a| {
            Ok(Value::Float(vector::norm(&vec_arg(&a[0])?) as f64))
        }),
        b(
            "normalize",
            1,
            Some(1),
            "scales a vector to unit length",
            |a| Ok(Value::Vector(vector::normalized(&vec_arg(&a[0])?))),
        ),
    ];

    pub fn install(r: &mut Registry) {
        for f in BUILTINS {
            let _ = r.register_fn(f.name, Arc::new(*f));
        }
    }
}
