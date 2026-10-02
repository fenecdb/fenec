//! `fenec types --lang python|go|csharp`: a row of each collection as the
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
}

impl Lang {
    pub fn named(s: &str) -> Option<Lang> {
        match s {
            "python" | "py" => Some(Lang::Python),
            "go" => Some(Lang::Go),
            "csharp" | "cs" | "c#" | "dotnet" => Some(Lang::CSharp),
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
