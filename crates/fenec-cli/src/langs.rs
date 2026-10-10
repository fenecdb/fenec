//! `fenec types --lang python|go|csharp|swift|kotlin|dart`: a row of each collection as the
//! language types JSON -- what each SDK's client reads a row into -- from a
//! database or a `schema.fenecql`, as `fenec types` writes TypeScript.
//!
//! The row as JSON carries it, not as the engine holds it: a timestamp is
//! ISO-8601 text, a vector an array of numbers, a sparse vector pgvector's
//! text, `bytes` an array of byte values, a `json` field any value -- and a
//! field that is not `required` reads `null`. `id` is every row's.
//! `integrations/types-golden/` holds each language's output for one
//! schema, which the tests make again.

use fenec_core::prelude::*;

/// A language `--lang` names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Python,
    Go,
    CSharp,
    Swift,
    Kotlin,
    Dart,
}

impl Lang {
    pub fn named(s: &str) -> Option<Lang> {
        match s {
            "python" | "py" => Some(Lang::Python),
            "go" => Some(Lang::Go),
            "csharp" | "cs" | "c#" | "dotnet" => Some(Lang::CSharp),
            "swift" => Some(Lang::Swift),
            "kotlin" | "kt" => Some(Lang::Kotlin),
            "dart" => Some(Lang::Dart),
            _ => None,
        }
    }
}

/// The file for `lang`; `name` is the Go package or the C# namespace.
pub fn render(lang: Lang, from: &str, name: &str, schemas: &[Schema]) -> String {
    let schemas: Vec<&Schema> = schemas
        .iter()
        .filter(|s| !s.name.starts_with('_'))
        .collect();
    match lang {
        Lang::Python => python(from, &schemas),
        Lang::Go => go(from, name, &schemas),
        Lang::CSharp => csharp(from, name, &schemas),
        Lang::Swift => swift(from, &schemas),
        Lang::Kotlin => kotlin(from, name, &schemas),
        Lang::Dart => dart(from, &schemas),
    }
}

fn header(comment: &str, from: &str, flag: &str, file: &str) -> String {
    format!(
        "{comment} generated from the fenecdb schema: {from}\n\
         {comment} fenec types --lang {flag} {from} > {file}\n\
         {comment} Do not edit by hand -- regenerate when the schema changes.\n"
    )
}

/// `title` -> `Title`, `product_id` -> `ProductId`: a type's or a member's
/// name, the field's own kept in the JSON tag beside it.
fn pascal(name: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in name.chars() {
        if c == '_' {
            up = true;
            continue;
        }
        match up {
            true => out.extend(c.to_uppercase()),
            false => out.push(c),
        }
        up = false;
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'F');
    }
    out
}

/// A name every type of one file takes once: a second gets a number.
fn unique(name: String, taken: &mut Vec<String>) -> String {
    let mut n = name.clone();
    let mut i = 2;
    while taken.contains(&n) {
        n = format!("{name}{i}");
        i += 1;
    }
    taken.push(n.clone());
    n
}

fn note(f: &fenec_core::schema::Field) -> String {
    let mut s = f.ty.name();
    if let Some(c) = f.collate {
        s.push_str(" collate ");
        s.push_str(c.name());
    }
    if let Some(ix) = fenec_core::declared::index_text(&f.index) {
        s.push(' ');
        s.push_str(&ix);
    }
    if f.required {
        s.push_str(" required");
    }
    s
}

// ------------------------------------------------------------------ Python

const PY_KEYWORDS: [&str; 35] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

fn py_type(t: &DataType) -> String {
    match t {
        DataType::Bool => "bool".into(),
        DataType::Int => "int".into(),
        DataType::Float => "float".into(),
        DataType::Text | DataType::Timestamp | DataType::Sparse(_) => "str".into(),
        DataType::Bytes => "list[int]".into(),
        DataType::Vector(..) => "list[float]".into(),
        DataType::Json => "Any".into(),
        DataType::Geo => "list[float]".into(),
        DataType::List(inner) => format!("list[{}]", py_type(inner)),
    }
}

/// A `TypedDict` a collection, the row as `json.loads` reads it; a name
/// Python cannot take as an attribute -- a keyword -- written in the
/// functional form.
fn python(from: &str, schemas: &[&Schema]) -> String {
    let mut out = header("#", from, "python", "fenec_schema.py");
    out.push_str("\nfrom typing import Any, Optional, TypedDict\n");
    let mut taken = Vec::new();
    for s in schemas {
        let class = unique(pascal(&s.name), &mut taken);
        let field = |f: &fenec_core::schema::Field| {
            let t = py_type(&f.ty);
            match f.required || f.ty == DataType::Json {
                true => t,
                false => format!("Optional[{t}]"),
            }
        };
        let functional = s
            .fields
            .iter()
            .any(|f| PY_KEYWORDS.contains(&f.name.as_str()));
        out.push_str("\n\n");
        if functional {
            out.push_str(&format!(
                "{class} = TypedDict('{class}', {{\n    'id': int,\n"
            ));
            for f in &s.fields {
                out.push_str(&format!("    '{}': {},  # {}\n", f.name, field(f), note(f)));
            }
            out.push_str(&format!(
                "}})\n{class}.__doc__ = 'A row of `{}`.'\n",
                s.name
            ));
            continue;
        }
        out.push_str(&format!(
            "class {class}(TypedDict):\n    \"\"\"A row of `{}`.\"\"\"\n\n    id: int\n",
            s.name
        ));
        for f in &s.fields {
            out.push_str(&format!("    {}: {}  # {}\n", f.name, field(f), note(f)));
        }
    }
    out
}

// ---------------------------------------------------------------------- Go

fn go_type(t: &DataType, json: &mut bool) -> String {
    match t {
        DataType::Bool => "bool".into(),
        DataType::Int => "int64".into(),
        DataType::Float => "float64".into(),
        DataType::Text | DataType::Timestamp | DataType::Sparse(_) => "string".into(),
        DataType::Bytes => "[]int".into(),
        DataType::Vector(..) => "[]float32".into(),
        DataType::Json => {
            *json = true;
            "json.RawMessage".into()
        }
        // `[lon, lat]`, which `encoding/json` reads into an array of two.
        DataType::Geo => "[2]float64".into(),
        DataType::List(inner) => format!("[]{}", go_type(inner, json)),
    }
}

/// A struct a collection, its fields tagged with their JSON names; a field
/// that reads `null` a pointer, but for a slice and a raw message, which
/// read `null` as nil already.
fn go(from: &str, package: &str, schemas: &[&Schema]) -> String {
    let mut json = false;
    let mut body = String::new();
    let mut taken = Vec::new();
    for s in schemas {
        let name = unique(pascal(&s.name), &mut taken);
        body.push_str(&format!(
            "\n// {name} is a row of `{}`.\ntype {name} struct {{\n",
            s.name
        ));
        let mut members = vec![(
            "ID".to_string(),
            "int64".to_string(),
            "id".to_string(),
            String::new(),
        )];
        let mut names = vec!["ID".to_string()];
        for f in &s.fields {
            let t = go_type(&f.ty, &mut json);
            let nil = t.starts_with("[]") || t == "json.RawMessage";
            let t = match f.required || nil {
                true => t,
                false => format!("*{t}"),
            };
            let m = unique(pascal(&f.name), &mut names);
            members.push((m, t, f.name.clone(), note(f)));
        }
        // Aligned in columns, as gofmt aligns them.
        let width = |s: &str| s.chars().count();
        let w = members.iter().map(|m| width(&m.0)).max().unwrap_or(0);
        let tw = members.iter().map(|m| m.1.len()).max().unwrap_or(0);
        let gw = members.iter().map(|m| width(&m.2) + 9).max().unwrap_or(0);
        for (m, t, tag, note) in members {
            let pad = " ".repeat(w - width(&m) + 1);
            let tpad = " ".repeat(tw - t.len() + 1);
            let tag = format!("`json:\"{tag}\"`");
            let line = match note.is_empty() {
                true => format!("\t{m}{pad}{t}{tpad}{tag}"),
                false => format!(
                    "\t{m}{pad}{t}{tpad}{tag}{} // {note}",
                    " ".repeat(gw - width(&tag))
                ),
            };
            body.push_str(line.trim_end());
            body.push('\n');
        }
        body.push_str("}\n");
    }
    let mut out = header("//", from, "go", "fenec_schema.go");
    out.push_str(&format!("\npackage {package}\n"));
    if json {
        out.push_str("\nimport \"encoding/json\"\n");
    }
    out.push_str(&body);
    out
}

// ---------------------------------------------------------------------- C#

fn cs_type(t: &DataType) -> String {
    match t {
        DataType::Bool => "bool".into(),
        DataType::Int => "long".into(),
        DataType::Float => "double".into(),
        DataType::Text | DataType::Timestamp | DataType::Sparse(_) => "string".into(),
        DataType::Bytes => "int[]".into(),
        DataType::Vector(..) => "float[]".into(),
        DataType::Json => "JsonElement".into(),
        DataType::Geo => "double[]".into(),
        DataType::List(inner) => format!("{}[]", cs_type(inner)),
    }
}

/// A record a collection, each member named for JSON by the field's name
/// (`System.Text.Json`); a field that reads `null` nullable.
fn csharp(from: &str, namespace: &str, schemas: &[&Schema]) -> String {
    let mut out = header("//", from, "csharp", "FenecSchema.cs");
    out.push_str(&format!(
        "\n#nullable enable\nusing System.Text.Json;\nusing System.Text.Json.Serialization;\n\nnamespace {namespace};\n"
    ));
    let mut taken = Vec::new();
    for s in schemas {
        let name = unique(pascal(&s.name), &mut taken);
        out.push_str(&format!(
            "\n/// <summary>A row of <c>{}</c>.</summary>\npublic sealed record {name}(\n",
            s.name
        ));
        let mut lines = vec!["    [property: JsonPropertyName(\"id\")] long Id".to_string()];
        let mut names = vec!["Id".to_string(), name.clone()];
        for f in &s.fields {
            let t = cs_type(&f.ty);
            let t = match f.required {
                true => t,
                false => format!("{t}?"),
            };
            let m = unique(pascal(&f.name), &mut names);
            lines.push(format!(
                "    /* {} */ [property: JsonPropertyName(\"{}\")] {t} {m}",
                note(f),
                f.name
            ));
        }
        out.push_str(&lines.join(",\n"));
        out.push_str(");\n");
    }
    out
}

/// `product_id` -> `productId`: a member's name where the language's own
/// is camel case, the field's kept beside it for the JSON.
fn camel(name: &str) -> String {
    let p = pascal(name);
    let mut c = p.chars();
    match c.next() {
        Some(first) => first.to_lowercase().chain(c).collect(),
        None => p,
    }
}

// ------------------------------------------------------------------- Swift

const SWIFT_KEYWORDS: [&str; 24] = [
    "as",
    "break",
    "case",
    "class",
    "continue",
    "default",
    "defer",
    "do",
    "else",
    "enum",
    "extension",
    "false",
    "for",
    "func",
    "if",
    "import",
    "in",
    "init",
    "is",
    "let",
    "nil",
    "return",
    "self",
    "struct",
];

fn swift_type(t: &DataType, json: &mut bool) -> String {
    match t {
        DataType::Bool => "Bool".into(),
        DataType::Int => "Int64".into(),
        DataType::Float => "Double".into(),
        DataType::Text | DataType::Timestamp | DataType::Sparse(_) => "String".into(),
        DataType::Bytes => "[UInt8]".into(),
        DataType::Vector(..) => "[Float]".into(),
        DataType::Json => {
            *json = true;
            "JSON".into()
        }
        DataType::Geo => "[Double]".into(),
        DataType::List(inner) => format!("[{}]", swift_type(inner, json)),
    }
}

/// A `Codable` struct a collection, what `Row.decode(as:)` reads a row
/// into; a member named apart from its field keeps it in `CodingKeys`, and
/// a `json` field is a `JSON` the file declares, as Swift has no value of
/// any JSON of its own.
fn swift(from: &str, schemas: &[&Schema]) -> String {
    let mut json = false;
    let mut body = String::new();
    let mut taken = Vec::new();
    for s in schemas {
        let name = unique(pascal(&s.name), &mut taken);
        body.push_str(&format!(
            "\n/// A row of `{}`.\npublic struct {name}: Codable, Sendable, Equatable {{\n    public var id: Int64\n",
            s.name
        ));
        let mut keys = vec!["id".to_string()];
        let mut renamed = false;
        let mut names = vec!["id".to_string()];
        for f in &s.fields {
            let t = swift_type(&f.ty, &mut json);
            let opt = if f.required { "" } else { "?" };
            let m = unique(camel(&f.name), &mut names);
            let shown = match SWIFT_KEYWORDS.contains(&m.as_str()) {
                true => format!("`{m}`"),
                false => m.clone(),
            };
            body.push_str(&format!(
                "    public var {shown}: {t}{opt}  // {}\n",
                note(f)
            ));
            match m == f.name {
                true => keys.push(shown),
                false => {
                    renamed = true;
                    keys.push(format!("{shown} = \"{}\"", f.name));
                }
            }
        }
        if renamed {
            body.push_str("\n    enum CodingKeys: String, CodingKey {\n");
            for k in keys {
                body.push_str(&format!("        case {k}\n"));
            }
            body.push_str("    }\n");
        }
        body.push_str("}\n");
    }
    let mut out = header("//", from, "swift", "FenecSchema.swift");
    out.push_str("\nimport Foundation\n");
    if json {
        out.push_str(
            "\n/// `json`: any value JSON holds.\n\
             public indirect enum JSON: Codable, Sendable, Equatable {\n\
             \x20   case null, bool(Bool), number(Double), string(String), array([JSON]), object([String: JSON])\n\n\
             \x20   public init(from decoder: Decoder) throws {\n\
             \x20       let c = try decoder.singleValueContainer()\n\
             \x20       if c.decodeNil() { self = .null }\n\
             \x20       else if let b = try? c.decode(Bool.self) { self = .bool(b) }\n\
             \x20       else if let n = try? c.decode(Double.self) { self = .number(n) }\n\
             \x20       else if let s = try? c.decode(String.self) { self = .string(s) }\n\
             \x20       else if let a = try? c.decode([JSON].self) { self = .array(a) }\n\
             \x20       else { self = .object(try c.decode([String: JSON].self)) }\n\
             \x20   }\n\n\
             \x20   public func encode(to encoder: Encoder) throws {\n\
             \x20       var c = encoder.singleValueContainer()\n\
             \x20       switch self {\n\
             \x20       case .null: try c.encodeNil()\n\
             \x20       case .bool(let b): try c.encode(b)\n\
             \x20       case .number(let n): try c.encode(n)\n\
             \x20       case .string(let s): try c.encode(s)\n\
             \x20       case .array(let a): try c.encode(a)\n\
             \x20       case .object(let o): try c.encode(o)\n\
             \x20       }\n\
             \x20   }\n\
             }\n",
        );
    }
    out.push_str(&body);
    out
}

// ------------------------------------------------------------------ Kotlin

const KOTLIN_KEYWORDS: [&str; 18] = [
    "as",
    "break",
    "class",
    "continue",
    "do",
    "else",
    "false",
    "for",
    "fun",
    "if",
    "in",
    "interface",
    "is",
    "null",
    "object",
    "return",
    "true",
    "val",
];

/// The Kotlin type, and how a value of it is read off the binding's `Row`
/// (`v` the value, `Any?`).
fn kotlin_type(t: &DataType) -> (String, String) {
    match t {
        DataType::Bool => ("Boolean".into(), "(v as Boolean)".into()),
        DataType::Int => ("Long".into(), "(v as Number).toLong()".into()),
        DataType::Float => ("Double".into(), "(v as Number).toDouble()".into()),
        DataType::Text | DataType::Timestamp | DataType::Sparse(_) => {
            ("String".into(), "(v as String)".into())
        }
        DataType::Bytes => (
            "List<Int>".into(),
            "(v as List<*>).map { (it as Number).toInt() }".into(),
        ),
        DataType::Vector(..) => (
            "List<Float>".into(),
            "(v as List<*>).map { (it as Number).toFloat() }".into(),
        ),
        DataType::Json => ("Any".into(), "v".into()),
        DataType::Geo => (
            "List<Double>".into(),
            "(v as List<*>).map { (it as Number).toDouble() }".into(),
        ),
        DataType::List(inner) => {
            let (t, read) = kotlin_type(inner);
            (
                format!("List<{t}>"),
                format!("(v as List<*>).map {{ v -> {read} }}"),
            )
        }
    }
}

/// A data class a collection, and `from(row)`, which reads one off the
/// binding's `Row`: a member named apart from its field reads it by the
/// field's name.
fn kotlin(from: &str, package: &str, schemas: &[&Schema]) -> String {
    let mut out = header("//", from, "kotlin", "FenecSchema.kt");
    out.push_str(&format!(
        "\npackage {}\n\nimport com.fenecdb.Row\n",
        package.to_lowercase()
    ));
    let mut taken = Vec::new();
    for s in schemas {
        let name = unique(pascal(&s.name), &mut taken);
        let mut members = vec!["    val id: Long,".to_string()];
        let mut reads = vec!["            id = (row[\"id\"] as Number).toLong(),".to_string()];
        let mut names = vec!["id".to_string()];
        for f in &s.fields {
            let (t, read) = kotlin_type(&f.ty);
            let m = unique(camel(&f.name), &mut names);
            let shown = match KOTLIN_KEYWORDS.contains(&m.as_str()) {
                true => format!("`{m}`"),
                false => m,
            };
            let nullable = !f.required || f.ty == DataType::Json;
            let t = if nullable { format!("{t}?") } else { t };
            members.push(format!("    val {shown}: {t}, // {}", note(f)));
            let key = f.name.replace('"', "\\\"");
            reads.push(match nullable {
                true => format!("            {shown} = row[\"{key}\"]?.let {{ v -> {read} }},"),
                false => format!("            {shown} = row[\"{key}\"]!!.let {{ v -> {read} }},"),
            });
        }
        out.push_str(&format!(
            "\n/** A row of `{}`. */\ndata class {name}(\n{}\n) {{\n    companion object {{\n        /** The row as the binding reads it. */\n        fun from(row: Row) = {name}(\n{}\n        )\n    }}\n}}\n",
            s.name,
            members.join("\n"),
            reads.join("\n")
        ));
    }
    out
}

// -------------------------------------------------------------------- Dart

const DART_KEYWORDS: [&str; 20] = [
    "assert", "break", "case", "catch", "class", "const", "continue", "default", "do", "else",
    "enum", "extends", "false", "final", "for", "if", "in", "is", "new", "null",
];

/// A Dart name: ASCII letters, digits and `_` alone, as Dart reads one.
fn dart_name(n: &str) -> String {
    let mut out: String = n
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) || DART_KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out
}

/// The Dart type, and how a value of it is read off a JSON map (`v`).
fn dart_type(t: &DataType) -> (String, String) {
    match t {
        DataType::Bool => ("bool".into(), "v as bool".into()),
        DataType::Int => ("int".into(), "v as int".into()),
        DataType::Float => ("double".into(), "(v as num).toDouble()".into()),
        DataType::Text | DataType::Timestamp | DataType::Sparse(_) => {
            ("String".into(), "v as String".into())
        }
        DataType::Bytes => (
            "List<int>".into(),
            "[for (final e in v as List) e as int]".into(),
        ),
        DataType::Vector(..) => (
            "List<double>".into(),
            "[for (final e in v as List) (e as num).toDouble()]".into(),
        ),
        DataType::Json => ("Object".into(), "v".into()),
        DataType::Geo => (
            "List<double>".into(),
            "[for (final e in v as List) (e as num).toDouble()]".into(),
        ),
        DataType::List(inner) => {
            let (t, read) = dart_type(inner);
            (
                format!("List<{t}>"),
                format!("[for (final v in v as List) {read}]"),
            )
        }
    }
}

/// A class a collection, and `fromJson`, which reads one off the map a row
/// is: a member named apart from its field reads it by the field's name.
fn dart(from: &str, schemas: &[&Schema]) -> String {
    let mut out = header("//", from, "dart", "fenec_schema.dart");
    let mut taken = Vec::new();
    for s in schemas {
        let name = unique(dart_name(&pascal(&s.name)), &mut taken);
        let mut fields = vec!["  final int id;".to_string()];
        let mut params = vec!["required this.id".to_string()];
        let mut reads = vec!["        id: j['id'] as int,".to_string()];
        let mut names = vec!["id".to_string()];
        for f in &s.fields {
            let (t, read) = dart_type(&f.ty);
            let m = unique(dart_name(&camel(&f.name)), &mut names);
            let nullable = !f.required || f.ty == DataType::Json;
            let shown_t = if nullable { format!("{t}?") } else { t };
            fields.push(format!("  /// {}\n  final {shown_t} {m};", note(f)));
            params.push(match nullable {
                true => format!("this.{m}"),
                false => format!("required this.{m}"),
            });
            let key = f.name.replace('\'', "\\'");
            reads.push(match nullable {
                true => format!(
                    "        {m}: switch (j['{key}']) {{ null => null, final v => {read} }},"
                ),
                false => format!("        {m}: switch (j['{key}']) {{ final v => {read} }},"),
            });
        }
        out.push_str(&format!(
            "\n/// A row of `{}`.\nclass {name} {{\n{}\n\n  const {name}({{{}}});\n\n  /// The row as JSON reads it.\n  factory {name}.fromJson(Map<String, Object?> j) => {name}(\n{}\n      );\n}}\n",
            s.name,
            fields.join("\n"),
            params.join(", "),
            reads.join("\n")
        ));
    }
    out
}
