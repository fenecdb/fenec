//! `fenec types` -- generates a TypeScript declaration from the live schema.
//!
//! Drizzle/Kysely make you define the schema in TypeScript *again*; the two
//! sides drift apart over time. Here the single source is the file itself:
//! the declaration comes out of the schema and is regenerated when it changes.
//!
//! ```text
//! fenec types data.fenec > web/fenec-schema.d.ts
//! ```

use fenec_core::prelude::*;

const USAGE: &str = "\
usage: fenec types <file.fenec> [-o <out.d.ts>] [--name <type name>]

  -o, --out <path>   write to a file (default: stdout)
      --name <name>  name of the generated type (default: FenecSchema)
";

pub fn main(args: &[String]) -> i32 {
    let mut path: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut name = "FenecSchema";

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "-o" | "--out" => {
                i += 1;
                match args.get(i) {
                    Some(v) => out = Some(v),
                    None => return fail("-o expects a path"),
                }
            }
            "--name" => {
                i += 1;
                match args.get(i) {
                    Some(v) => name = v,
                    None => return fail("--name expects a name"),
                }
            }
            other if !other.starts_with('-') => path = Some(other),
            other => return fail(&format!("unknown option: {other}")),
        }
        i += 1;
    }

    let Some(path) = path else {
        eprintln!("{USAGE}");
        return 2;
    };

    let db = match fenec_core::fs::open(path) {
        Ok(db) => db,
        Err(e) => return fail(&format!("could not open {path}: {e}")),
    };

    let mut schemas = Vec::new();
    for n in db.collection_names() {
        match db.collection(&n) {
            Ok(c) => schemas.push(c.schema.clone()),
            Err(e) => return fail(&format!("{n}: {e}")),
        }
    }

    let src = render(path, name, &schemas);
    match out {
        Some(p) => match std::fs::write(p, &src) {
            Ok(()) => eprintln!("{p}  {} collections", schemas.len()),
            Err(e) => return fail(&format!("could not write {p}: {e}")),
        },
        None => print!("{src}"),
    }
    0
}

fn fail(msg: &str) -> i32 {
    eprintln!("{msg}");
    2
}

/// Produces the `.d.ts` body from the schema.
fn render(path: &str, name: &str, schemas: &[Schema]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "// generated from the fenecdb schema: {path}\n\
         // fenec types {path} > fenec-schema.d.ts\n\
         // Do not edit by hand -- regenerate when the schema changes.\n\n"
    ));

    // Only the branded types that are actually used are written.
    let used = |f: fn(&DataType) -> bool| {
        schemas
            .iter()
            .flat_map(|s| s.fields.iter())
            .any(|fd| f(&fd.ty))
    };
    // The brands are structural: they are identical to the definitions in
    // `web/fenec.d.ts`, so values pass freely between the two with no import.
    if used(|t| matches!(t, DataType::Timestamp) || matches!(t, DataType::List(i) if **i == DataType::Timestamp))
    {
        out.push_str(
            "/** `timestamp`: ISO-8601 text when read; a Date/number is accepted when writing. */\n\
             export type Timestamp = string & { readonly __fenec: 'timestamp' };\n",
        );
    }
    if used(|t| matches!(t, DataType::Vector(..))) {
        out.push_str(
            "/** `vector<N>`: an array of numbers in JSON. */\n\
             export type Vector = number[] & { readonly __fenec: 'vector' };\n",
        );
    }
    if used(|t| matches!(t, DataType::Bytes) || matches!(t, DataType::List(i) if **i == DataType::Bytes))
    {
        out.push_str(
            "/** `bytes`: an array of bytes in JSON. */\n\
             export type Bytes = number[] & { readonly __fenec: 'bytes' };\n",
        );
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }

    if schemas.is_empty() {
        out.push_str(&format!(
            "// (there are no collections in the file)\nexport type {name} = {{}};\n"
        ));
        return out;
    }

    out.push_str(&format!("export type {name} = {{\n"));
    for (i, s) in schemas.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("  {}: {{\n", key(&s.name)));
        for f in &s.fields {
            // The schema line is kept in a comment: the type name loses
            // information (`vector<768> @hnsw(cosine)` -> `Vector`).
            let mut note = f.ty.name();
            if let Some(ix) = index_note(&f.index) {
                note.push_str(&ix);
            }
            if f.required {
                note.push_str(" required");
            }
            out.push_str(&format!("    /** {note} */\n"));
            // `id` is automatic and added by `Row<F>`.
            // A field that is not required reads back as `null` when unwritten.
            let nullable = if f.required { "" } else { " | null" };
            out.push_str(&format!(
                "    {}: {}{nullable};\n",
                key(&f.name),
                ts_type(&f.ty)
            ));
        }
        out.push_str("  };\n");
    }
    out.push_str("};\n");
    out
}

fn ts_type(t: &DataType) -> String {
    match t {
        DataType::Bool => "boolean".into(),
        DataType::Int | DataType::Float => "number".into(),
        DataType::Text => "string".into(),
        DataType::Bytes => "Bytes".into(),
        DataType::Timestamp => "Timestamp".into(),
        DataType::Vector(..) => "Vector".into(),
        DataType::List(inner) => format!("{}[]", ts_type(inner)),
    }
}

fn index_note(k: &IndexKind) -> Option<String> {
    match k {
        IndexKind::None => None,
        IndexKind::Hash => Some(" @hash".into()),
        IndexKind::Vector(spec) => Some(format!(
            " @hnsw({}, m={}, ef_search={})",
            spec.metric.name(), spec.m, spec.ef_search
        )),
    }
}

/// A TypeScript key: quoted when it is not an ASCII identifier. FenecQL names
/// accept Unicode letters (`año`, `résumé`), and so does TS, but quoting is
/// valid either way and `keyof` works unchanged.
fn key(name: &str) -> String {
    let ascii_ident = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ascii_ident {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fenec_core::schema::{Field, VectorIndexSpec};
    use fenec_core::value::VecPrec;

    fn schema() -> Schema {
        Schema::new(
            "articles",
            vec![
                Field::new("title", DataType::Text).required(),
                Field::new("tags", DataType::List(Box::new(DataType::Text))),
                Field::new("year", DataType::Int).indexed(IndexKind::Hash),
                Field::new("published", DataType::Timestamp),
                Field::new("embed", DataType::Vector(768, VecPrec::F32))
                    .indexed(IndexKind::Vector(VectorIndexSpec::default())),
            ],
        )
        .unwrap()
    }

    #[test]
    fn renders_declaration() {
        let out = render("data.fenec", "FenecSchema", &[schema()]);
        assert!(out.contains("export type FenecSchema = {"));
        assert!(out.contains("  articles: {"));
        // A required field takes no null, the others do.
        assert!(out.contains("    title: string;"));
        assert!(out.contains("    tags: string[] | null;"));
        assert!(out.contains("    year: number | null;"));
        assert!(out.contains("    published: Timestamp | null;"));
        assert!(out.contains("    embed: Vector | null;"));
        // `id` is automatic, added by `Row<F>`.
        assert!(!out.contains("id:"));
        // Branded types that are used are written, unused ones are not.
        assert!(out.contains("export type Timestamp"));
        assert!(out.contains("export type Vector"));
        assert!(!out.contains("export type Bytes"));
        // The schema line survives in a comment.
        assert!(out.contains("@hnsw(cosine"), "{out}");
        assert!(out.contains("/** int @hash */"), "{out}");
    }

    #[test]
    fn empty_file() {
        let out = render("empty.fenec", "FenecSchema", &[]);
        assert!(out.contains("export type FenecSchema = {};"));
    }

    #[test]
    fn quotes_non_ascii_keys() {
        assert_eq!(key("title"), "title");
        assert_eq!(key("_a1"), "_a1");
        assert_eq!(key("summary"), "summary");
        assert_eq!(key("r\u{e9}sum\u{e9}"), "'r\u{e9}sum\u{e9}'");
    }
}
