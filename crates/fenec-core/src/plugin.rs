//! Plugin system -- inspired by PostgreSQL's extension model.
//!
//! A plugin can hook into:
//!   * scalar functions (`register_fn`)  -> callable in FenecQL expressions
//!   * write hooks      (`register_hook`) -> trigger-like
//!   * export adapters                    -> e.g. the PostgreSQL wire protocol
//!
//! The core knows nothing about plugins; fenec-pg uses this interface to make
//! fenecdb speak like a PostgreSQL server.

use crate::error::{Error, Result};
use crate::value::{Document, Value};
use std::collections::HashMap;
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
    /// Called before a write. The document may be modified (embedding
    /// generation, for instance). Returning `Err` aborts the write.
    fn before_write(&self, _collection: &str, _op: WriteOp, _doc: &mut Document) -> Result<()> {
        Ok(())
    }
    fn after_write(&self, _collection: &str, _op: WriteOp, _doc: &Document) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct Registry {
    functions: HashMap<String, Arc<dyn ScalarFn>>,
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
        if self.functions.contains_key(name) {
            return Err(Error::Exists(format!(
                "function `{name}` is already registered"
            )));
        }
        self.functions.insert(name.to_ascii_lowercase(), f);
        Ok(())
    }

    pub fn register_hook(&mut self, h: Arc<dyn Hook>) {
        self.hooks.push(h);
    }

    pub fn function(&self, name: &str) -> Option<&Arc<dyn ScalarFn>> {
        self.functions.get(&name.to_ascii_lowercase())
    }

    pub fn function_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.functions.keys().cloned().collect();
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
        let f = self
            .function(name)
            .ok_or_else(|| Error::Query(format!("unknown function `{name}`")))?;
        let (lo, hi) = f.arity();
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

    macro_rules! reg {
        ($r:expr, $name:literal, $lo:expr, $hi:expr, $doc:literal, $f:expr) => {{
            struct F;
            impl ScalarFn for F {
                fn call(&self, args: &[Value]) -> Result<Value> {
                    let f: fn(&[Value]) -> Result<Value> = $f;
                    f(args)
                }
                fn arity(&self) -> (usize, Option<usize>) {
                    ($lo, $hi)
                }
                fn doc(&self) -> &str {
                    $doc
                }
            }
            let _ = $r.register_fn($name, Arc::new(F));
        }};
    }

    fn vec_arg(v: &Value) -> Result<Vec<f32>> {
        match v {
            Value::Vector(x) => Ok(x.clone()),
            Value::List(items) => items
                .iter()
                .map(|i| {
                    i.as_f64()
                        .map(|f| f as f32)
                        .ok_or_else(|| Error::Type("vector element must be numeric".into()))
                })
                .collect(),
            other => Err(Error::Type(format!(
                "expected a vector, found {}",
                other.type_name()
            ))),
        }
    }

    pub fn install(r: &mut Registry) {
        reg!(r, "lower", 1, Some(1), "lowercases text", |a| {
            Ok(match &a[0] {
                Value::Text(s) => Value::Text(s.to_lowercase()),
                v => v.clone(),
            })
        });
        reg!(r, "upper", 1, Some(1), "uppercases text", |a| {
            Ok(match &a[0] {
                Value::Text(s) => Value::Text(s.to_uppercase()),
                v => v.clone(),
            })
        });
        reg!(r, "len", 1, Some(1), "length of text/list/vector", |a| {
            Ok(Value::Int(match &a[0] {
                Value::Text(s) => s.chars().count() as i64,
                Value::Bytes(b) => b.len() as i64,
                Value::List(l) => l.len() as i64,
                Value::Vector(v) => v.len() as i64,
                _ => 0,
            }))
        });
        reg!(
            r,
            "coalesce",
            1,
            None,
            "returns the first non-null value",
            |a| {
                Ok(a.iter()
                    .find(|v| !v.is_null())
                    .cloned()
                    .unwrap_or(Value::Null))
            }
        );
        reg!(
            r,
            "cosine",
            2,
            Some(2),
            "cosine similarity between two vectors",
            |a| {
                let (x, y) = (vec_arg(&a[0])?, vec_arg(&a[1])?);
                if x.len() != y.len() {
                    return Err(Error::Type("vector dimensions do not match".into()));
                }
                let (nx, ny) = (vector::normalized(&x), vector::normalized(&y));
                Ok(Value::Float(vector::dot(&nx, &ny) as f64))
            }
        );
        reg!(
            r,
            "l2",
            2,
            Some(2),
            "euclidean distance between two vectors",
            |a| {
                let (x, y) = (vec_arg(&a[0])?, vec_arg(&a[1])?);
                if x.len() != y.len() {
                    return Err(Error::Type("vector dimensions do not match".into()));
                }
                Ok(Value::Float(vector::l2_sq(&x, &y).sqrt() as f64))
            }
        );
        reg!(r, "dot", 2, Some(2), "inner product", |a| {
            let (x, y) = (vec_arg(&a[0])?, vec_arg(&a[1])?);
            if x.len() != y.len() {
                return Err(Error::Type("vector dimensions do not match".into()));
            }
            Ok(Value::Float(vector::dot(&x, &y) as f64))
        });
        reg!(r, "now", 0, Some(0), "current time (UTC)", |_a| {
            Ok(Value::Timestamp(crate::time::now_ms()?))
        });
        reg!(
            r,
            "timestamp",
            1,
            Some(1),
            "converts text or epoch milliseconds into a timestamp",
            |a| { a[0].clone().coerce(&crate::value::DataType::Timestamp) }
        );
        reg!(r, "norm", 1, Some(1), "vector length (L2 norm)", |a| {
            Ok(Value::Float(vector::norm(&vec_arg(&a[0])?) as f64))
        });
        reg!(
            r,
            "normalize",
            1,
            Some(1),
            "scales a vector to unit length",
            |a| { Ok(Value::Vector(vector::normalized(&vec_arg(&a[0])?))) }
        );
    }
}
