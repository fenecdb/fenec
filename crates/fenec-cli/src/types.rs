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
use fenec_core::schema::{Quant, TextIndexSpec, VectorIndexSpec};
use fenec_core::value::VecPrec;

const USAGE: &str = "\
usage: fenec types <file.fenec | schema.fenecql> [-o <out>] [--name <name>]
       fenec types --lang python|go|csharp|swift|kotlin|dart <file> [-o <out>] [--name <package>]
       fenec types --schema <file> [-o <schema.ts>] [--import <module>]
       fenec types --fenecql <file> [-o <schema.fenecql>]

  <file>               a database, or a schema written as FenecQL (.fenecql):
                       `create collection` and `create index` statements
  -o, --out <path>     write to a file (default: stdout)
      --name <name>    the generated type (default: FenecSchema), the Go
                       package (fenecschema) or the C# namespace (FenecSchema)
      --lang <lang>    a row of each collection in Python (TypedDict), Go
                       (structs), C# (records), Swift (Codable structs), Kotlin
                       (data classes) or Dart (fromJson) rather than TypeScript
      --schema         the tables as code (@fenecdb/web/schema), for a project
                       that declares its schema from here on
      --import <from>  where --schema's file imports the builders from
                       (default: @fenecdb/web/schema)
      --fenecql        the schema as FenecQL, which every SDK opens a
                       database with (`schema`) and `fenec types` reads
";

pub fn main(args: &[String]) -> i32 {
    let mut path: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut name = "FenecSchema";
    let mut tables = false;
    let mut fenecql = false;
    let mut lang = None;
    let mut named = false;
    let mut import = "@fenecdb/web/schema";

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            "--schema" => tables = true,
            "--fenecql" => fenecql = true,
            "--lang" => {
                i += 1;
                match args.get(i).map(|l| (l, crate::langs::Lang::named(l))) {
                    Some((_, Some(l))) => lang = Some(l),
                    Some((l, None)) if l == "ts" || l == "typescript" => {}
                    _ => {
                        return fail("--lang expects ts, python, go, csharp, swift, kotlin or dart")
                    }
                }
            }
            "--import" => {
                i += 1;
                match args.get(i) {
                    Some(v) => import = v,
                    None => return fail("--import expects a module"),
                }
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
                    Some(v) => {
                        name = v;
                        named = true;
                    }
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

    let schemas = match read(path) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let src = match (tables, fenecql, lang) {
        (true, false, None) => render_tables(path, import, &schemas),
        (false, true, None) => render_fenecql(path, &schemas),
        (false, false, Some(lang)) => {
            let default = match lang {
                crate::langs::Lang::Go | crate::langs::Lang::Kotlin => "fenecschema",
                _ => "FenecSchema",
            };
            crate::langs::render(lang, path, if named { name } else { default }, &schemas)
        }
        (false, false, None) => render(path, name, &schemas),
        _ => return fail("one of --schema, --fenecql and --lang"),
    };
    match out {
        Some(p) => match std::fs::write(p, &src) {
            Ok(()) => eprintln!("{p}  {} collections", schemas.len()),
            Err(e) => return fail(&format!("could not write {p}: {e}")),
        },
        None => print!("{src}"),
    }
    0
}

/// The schemas a database holds, or a `.fenecql` file declares.
pub fn read(path: &str) -> std::result::Result<Vec<Schema>, String> {
    if path.ends_with(".fenecql") {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))?;
        return fenec_ql::schema_text(&text).map_err(|e| format!("{path}: {e}"));
    }
    // Read only: the file is often a running server's, and an open that may
    // cut or create would write to it.
    let db =
        fenec_core::fs::open_read_only(path).map_err(|e| format!("could not open {path}: {e}"))?;
    let mut schemas = Vec::new();
    for n in db.collection_names() {
        let c = db.collection(&n).map_err(|e| format!("{n}: {e}"))?;
        schemas.push(c.schema.clone());
    }
    Ok(schemas)
}

/// `fenec types --fenecql`: the schema as FenecQL, every option written --
/// what an SDK opens its database with, and `fenec types` reads.
fn render_fenecql(path: &str, schemas: &[Schema]) -> String {
    format!(
        "-- generated from the fenecdb schema: {path}\n\
         -- fenec types --fenecql {path} > schema.fenecql\n\
         -- The schema as FenecQL: an open with it (`schema`) checks the database\n\
         -- against it, and `fenec types schema.fenecql` writes its types.\n\n{}",
        fenec_core::declared::fenecql(schemas)
    )
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
    if used(|t| {
        matches!(t, DataType::Timestamp)
            || matches!(t, DataType::List(i) if **i == DataType::Timestamp)
    }) {
        out.push_str(
            "/** `timestamp`: ISO-8601 text when read; a Date/number is accepted when writing. */\n\
             export type Timestamp = string & { readonly __fenec: 'timestamp' };\n",
        );
    }
    if used(|t| matches!(t, DataType::Sparse(_))) {
        out.push_str(
            "/** `sparse<N>`: pgvector's text form, `{1:0.5,3:0.25}/N`, indices from 1. */\n\
             export type Sparse = string & { readonly __fenec: 'sparse' };\n",
        );
    }
    if used(|t| matches!(t, DataType::Vector(..))) {
        out.push_str(
            "/** `vector<N>`: an array of numbers in JSON. */\n\
             export type Vector = number[] & { readonly __fenec: 'vector' };\n",
        );
    }
    if used(|t| {
        matches!(t, DataType::Geo) || matches!(t, DataType::List(i) if **i == DataType::Geo)
    }) {
        out.push_str(
            "/** `geo`: a point, its longitude and latitude in degrees. */\n\
             export type Point = [lon: number, lat: number];\n",
        );
    }
    if used(|t| matches!(t, DataType::Json)) {
        out.push_str(
            "/** `json`: any value JSON holds; a path reads into it, `'meta.lang'`. */\n\
             export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };\n",
        );
    }
    if used(|t| {
        matches!(t, DataType::Bytes) || matches!(t, DataType::List(i) if **i == DataType::Bytes)
    }) {
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

/// The builders `--schema`'s file may import: a table's variable takes
/// another name rather than shadow one.
const BUILDERS: [&str; 13] = [
    "fenecTable",
    "text",
    "integer",
    "doublePrecision",
    "boolean",
    "timestamp",
    "bytea",
    "json",
    "vector",
    "halfvec",
    "sparsevec",
    "index",
    "uniqueIndex",
];

/// Words a JavaScript `const` cannot be named.
const RESERVED: [&str; 38] = [
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "let",
    "yield",
];

/// `fenec types --schema`: the tables as code, Drizzle's way, which a
/// project that moves from reading its schema out of the database to
/// declaring it starts from -- `drizzle-kit pull`'s file. Opened with it,
/// the database holds what the code declares, to the last option: every
/// option that is not the engine's default is written.
fn render_tables(path: &str, import: &str, schemas: &[Schema]) -> String {
    let schemas: Vec<&Schema> = schemas
        .iter()
        .filter(|s| !s.name.starts_with('_'))
        .collect();
    let mut body = String::new();
    let mut used: Vec<&str> = vec!["fenecTable"];
    let mut names = Vec::new();
    for s in &schemas {
        let var = variable(&s.name, &names);
        names.push(var.clone());
        body.push_str(&format!(
            "\nexport const {var} = fenecTable('{}', {{\n",
            s.name
        ));
        let mut indexes = Vec::new();
        for f in &s.fields {
            let col = column(&f.ty, &mut used);
            let mut chain = col;
            if let Some(c) = f.collate {
                // Before `.array()`: the collation is the text's.
                chain = match chain.strip_suffix(".array()") {
                    Some(base) => format!("{base}.collate('{}').array()", c.name()),
                    None => format!("{chain}.collate('{}')", c.name()),
                };
            }
            if f.required {
                chain.push_str(".notNull()");
            }
            if f.index == IndexKind::UNIQUE {
                chain.push_str(".unique()");
            } else if let Some(ix) =
                index_code(s, &f.name, &member(&f.name), &f.ty, &f.index, &mut used)
            {
                indexes.push(ix);
            }
            body.push_str(&format!("  {}: {chain},\n", key(&f.name)));
        }
        for p in &s.paths {
            let Some((field, keys)) = fenec_core::schema::split_path(&p.name) else {
                continue;
            };
            let target = format!("{}.path('{keys}')", member(field));
            if let Some(ix) = index_code(s, &p.name, &target, &p.ty, &p.index, &mut used) {
                indexes.push(ix);
            }
        }
        match indexes.is_empty() {
            true => body.push_str("});\n"),
            false => {
                body.push_str("}, (t) => [\n");
                for ix in indexes {
                    body.push_str(&format!("  {ix},\n"));
                }
                body.push_str("]);\n");
            }
        }
    }
    let imports: Vec<&str> = BUILDERS
        .iter()
        .copied()
        .filter(|b| used.contains(b))
        .collect();
    let mut out = format!(
        "// generated from the fenecdb schema: {path}\n\
         // fenec types --schema {path} > schema.ts\n\
         // The tables are the code's from here on: change them here, and an open\n\
         // with them checks the database against them (Fenec.open's `schema`).\n\n\
         import {{ {} }} from '{import}';\n",
        imports.join(", ")
    );
    if schemas.is_empty() {
        out.push_str("\n// (there are no collections in the file)\n");
    }
    out.push_str(&body);
    out
}

/// A table's variable: its name where JavaScript takes it, else made so.
fn variable(name: &str, taken: &[String]) -> String {
    let ascii = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut v = match ascii {
        true => name.to_string(),
        false => name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect(),
    };
    if BUILDERS.contains(&v.as_str())
        || RESERVED.contains(&v.as_str())
        || v.starts_with(|c: char| c.is_ascii_digit())
    {
        v.push_str("Table");
    }
    while taken.contains(&v) {
        v.push('_');
    }
    v
}

/// A column of the callback's `t`: `t.title`, or `t['año']`.
fn member(name: &str) -> String {
    let k = key(name);
    match k.starts_with('\'') {
        true => format!("t[{k}]"),
        false => format!("t.{k}"),
    }
}

/// The builder a type is made by.
fn column(t: &DataType, used: &mut Vec<&'static str>) -> String {
    let (b, args): (&'static str, String) = match t {
        DataType::Bool => ("boolean", String::new()),
        DataType::Int => ("integer", String::new()),
        DataType::Float => ("doublePrecision", String::new()),
        DataType::Text => ("text", String::new()),
        DataType::Bytes => ("bytea", String::new()),
        DataType::Timestamp => ("timestamp", String::new()),
        DataType::Json => ("json", String::new()),
        DataType::Vector(n, VecPrec::F32) => ("vector", format!("{{ dimensions: {n} }}")),
        DataType::Vector(n, VecPrec::F16) => ("halfvec", format!("{{ dimensions: {n} }}")),
        DataType::Sparse(n) => ("sparsevec", format!("{{ dimensions: {n} }}")),
        DataType::Geo => ("geometry", "{ type: 'point' }".into()),
        DataType::List(inner) => return format!("{}.array()", column(inner, used)),
    };
    if !used.contains(&b) {
        used.push(b);
    }
    format!("{b}({args})")
}

/// An index as the callback declares it, named `<table>_<field>_idx` --
/// fenecdb keeps no name, Drizzle wants one.
fn index_code(
    s: &Schema,
    field: &str,
    target: &str,
    ty: &DataType,
    k: &IndexKind,
    used: &mut Vec<&'static str>,
) -> Option<String> {
    let name = format!("{}_{}_idx", s.name, field.replace('.', "_"));
    let mut use_ = |b: &'static str| {
        if !used.contains(&b) {
            used.push(b);
        }
    };
    Some(match k {
        IndexKind::None => return None,
        IndexKind::Hash { unique: true } => {
            use_("uniqueIndex");
            format!("uniqueIndex('{name}').on({target})")
        }
        IndexKind::Hash { unique: false } => {
            use_("index");
            format!("index('{name}').using('hash', {target})")
        }
        IndexKind::Sorted { ttl } => {
            use_("index");
            let mut s = format!("index('{name}').on({target})");
            if let Some(ms) = ttl {
                s.push_str(&format!(".ttl('{}')", fenec_core::schema::ttl_text(*ms)));
            }
            s
        }
        IndexKind::Inverted => {
            use_("index");
            format!("index('{name}').using('inverted', {target})")
        }
        // PostGIS's spatial index, as Drizzle declares one over a point.
        IndexKind::Geo => {
            use_("index");
            format!("index('{name}').using('gist', {target})")
        }
        IndexKind::Vector(spec) => {
            use_("index");
            let half = matches!(ty, DataType::Vector(_, VecPrec::F16));
            let op = match (spec.metric.name(), half) {
                ("l2", false) => "vector_l2_ops",
                ("dot", false) => "vector_ip_ops",
                (_, false) => "vector_cosine_ops",
                ("l2", true) => "halfvec_l2_ops",
                ("dot", true) => "halfvec_ip_ops",
                (_, true) => "halfvec_cosine_ops",
            };
            let default = VectorIndexSpec {
                metric: spec.metric,
                quant: spec.quant,
                ..VectorIndexSpec::default()
            }
            .resolved();
            let mut with = Vec::new();
            if spec.m != default.m {
                with.push(format!("m: {}", spec.m));
            }
            if spec.ef_construction != default.ef_construction {
                with.push(format!("ef_construction: {}", spec.ef_construction));
            }
            if spec.ef_search != default.ef_search {
                with.push(format!("ef_search: {}", spec.ef_search));
            }
            if spec.quant != Quant::None {
                with.push(format!("quant: '{}'", spec.quant.name()));
            }
            let mut s = format!("index('{name}').using('hnsw', {target}.op('{op}'))");
            if !with.is_empty() {
                s.push_str(&format!(".with({{ {} }})", with.join(", ")));
            }
            s
        }
        IndexKind::Text(spec) => {
            use_("index");
            let d = TextIndexSpec::default();
            let mut with = Vec::new();
            let mut num = |label: &str, v: f32| {
                let mut s = format!("{label}: ");
                fenec_core::num::f32_into(&mut s, v);
                with.push(s);
            };
            if spec.k1_pct != d.k1_pct {
                num("k1", spec.k1());
            }
            if spec.b_pct != d.b_pct {
                num("b", spec.b());
            }
            if spec.prefix_max != 0 {
                with.push(format!("prefix: {}", spec.prefix_max));
                if spec.prefix_min != d.prefix_min {
                    with.push(format!("prefix_min: {}", spec.prefix_min));
                }
            }
            if spec.chars {
                with.push("chars: true".into());
            }
            let mut s = format!("index('{name}').using('bm25', {target})");
            if !with.is_empty() {
                s.push_str(&format!(".with({{ {} }})", with.join(", ")));
            }
            s
        }
    })
}

fn ts_type(t: &DataType) -> String {
    match t {
        DataType::Bool => "boolean".into(),
        DataType::Int | DataType::Float => "number".into(),
        DataType::Text => "string".into(),
        DataType::Bytes => "Bytes".into(),
        DataType::Timestamp => "Timestamp".into(),
        DataType::Vector(..) => "Vector".into(),
        // pgvector's text form, `{1:0.5}/30522`: the string every transport
        // carries a sparse vector as.
        DataType::Sparse(_) => "Sparse".into(),
        DataType::List(inner) => format!("{}[]", ts_type(inner)),
        // Any value JSON holds: an object, a list, a number, text, a
        // boolean or null.
        DataType::Json => "Json".into(),
        // `[lon, lat]`, as a point is written and read.
        DataType::Geo => "Point".into(),
    }
}

fn index_note(k: &IndexKind) -> Option<String> {
    match k {
        IndexKind::None => None,
        IndexKind::Hash { unique: false } => Some(" @hash".into()),
        IndexKind::Hash { unique: true } => Some(" @unique".into()),
        IndexKind::Sorted { ttl: None } => Some(" @sorted".into()),
        IndexKind::Sorted { ttl: Some(ms) } => {
            Some(format!(" @ttl({})", fenec_core::schema::ttl_text(*ms)))
        }
        IndexKind::Vector(spec) => Some(format!(
            " @hnsw({}, m={}, ef_search={}{})",
            spec.metric.name(),
            spec.m,
            spec.ef_search,
            spec.quant_arg()
        )),
        IndexKind::Text(spec) => Some(format!(" @text({})", spec.args())),
        IndexKind::Inverted => Some(" @inverted".into()),
        IndexKind::Geo => Some(" @geo".into()),
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
                Field::new("year", DataType::Int).indexed(IndexKind::HASH),
                Field::new("published", DataType::Timestamp),
                Field::new("embed", DataType::Vector(768, VecPrec::F32))
                    .indexed(IndexKind::Vector(VectorIndexSpec::default())),
                Field::new("meta", DataType::Json),
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
        // A json field holds any value JSON does, the type `fenec.d.ts`
        // declares the same way, so the two pass between each other.
        assert!(out.contains("    meta: Json | null;"));
        assert!(out.contains(
            "export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };"
        ));
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

    /// integrations/types-golden: what each generator writes for one
    /// schema, read from its `.fenecql` file as `fenec types` reads it.
    /// `FENEC_TYPES_GOLDEN=write` writes the files again. Where Python, Go
    /// or .NET is installed, the file is compiled as well.
    #[test]
    fn every_generator_writes_what_types_golden_holds() {
        use crate::langs::{render as lang, Lang};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../integrations/types-golden");
        let from = "schema.fenecql";
        let schemas = read(dir.join(from).to_str().unwrap()).unwrap();
        let files = [
            ("fenec-schema.d.ts", render(from, "FenecSchema", &schemas)),
            (
                "tables.ts",
                render_tables(from, "@fenecdb/web/schema", &schemas),
            ),
            ("schema.out.fenecql", render_fenecql(from, &schemas)),
            (
                "fenec_schema.py",
                lang(Lang::Python, from, "FenecSchema", &schemas),
            ),
            (
                "fenec_schema.go",
                lang(Lang::Go, from, "fenecschema", &schemas),
            ),
            (
                "FenecSchema.cs",
                lang(Lang::CSharp, from, "FenecSchema", &schemas),
            ),
            ("FenecSchema.swift", lang(Lang::Swift, from, "", &schemas)),
            (
                "FenecSchema.kt",
                lang(Lang::Kotlin, from, "fenecschema", &schemas),
            ),
            ("fenec_schema.dart", lang(Lang::Dart, from, "", &schemas)),
        ];
        let write = std::env::var("FENEC_TYPES_GOLDEN").is_ok_and(|v| v == "write");
        for (name, made) in &files {
            let path = dir.join(name);
            if write {
                std::fs::write(&path, made).unwrap();
                continue;
            }
            let kept = std::fs::read_to_string(&path).unwrap_or_default();
            assert_eq!(
                &kept, made,
                "{name}: FENEC_TYPES_GOLDEN=write cargo test -p fenec-cli types"
            );
        }
        // The FenecQL a database is pulled into reads back as the same schemas.
        let pulled = fenec_ql::schema_text(&render_fenecql(from, &schemas)).unwrap();
        assert_eq!(pulled, schemas);
        // Each file is the language's own, where the language is here to say.
        let ok = |cmd: &mut std::process::Command| {
            cmd.output().map(|o| {
                (
                    o.status.success(),
                    String::from_utf8_lossy(&o.stderr).into_owned(),
                )
            })
        };
        if let Ok((true, _)) = ok(std::process::Command::new("python3").arg("--version")) {
            let r = ok(std::process::Command::new("python3")
                .args(["-c", "import ast,sys; ast.parse(open(sys.argv[1]).read())"])
                .arg(dir.join("fenec_schema.py")));
            assert!(matches!(r, Ok((true, _))), "python: {r:?}");
        }
        if let Ok(o) = std::process::Command::new("swiftc")
            .args(["-typecheck"])
            .arg(dir.join("FenecSchema.swift"))
            .output()
        {
            assert!(
                o.status.success(),
                "swiftc: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        if let Ok(o) = std::process::Command::new("dart")
            .args(["analyze", "--fatal-infos"])
            .arg(dir.join("fenec_schema.dart"))
            .output()
        {
            assert!(
                o.status.success(),
                "dart: {}",
                String::from_utf8_lossy(&o.stdout)
            );
        }
        if let Ok(o) = std::process::Command::new("gofmt")
            .arg("-d")
            .arg(dir.join("fenec_schema.go"))
            .output()
        {
            assert!(
                o.status.success() && o.stdout.is_empty(),
                "gofmt: {}",
                String::from_utf8_lossy(&o.stdout)
            );
        }
    }
}
