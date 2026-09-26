//! FenecQL lexer. Allocation is kept to a minimum: every token looks into the
//! source slice, and a String is only produced for strings/identifiers.

use fenec_core::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    /// `$1` -> Param(0)
    Param(usize),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Colon,
    At,
    Star,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    Tilde,
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Str(s) => format!("\"{s}\""),
            Tok::Int(i) => i.to_string(),
            Tok::Float(f) => {
                let mut text = String::new();
                fenec_core::num::f64_into(&mut text, *f);
                text
            }
            Tok::Param(i) => format!("${}", i + 1),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Colon => "`:`".into(),
            Tok::At => "`@`".into(),
            Tok::Star => "`*`".into(),
            Tok::Lt => "`<`".into(),
            Tok::Le => "`<=`".into(),
            Tok::Gt => "`>`".into(),
            Tok::Ge => "`>=`".into(),
            Tok::Eq => "`=`".into(),
            Tok::Ne => "`!=`".into(),
            Tok::Tilde => "`~`".into(),
            Tok::Eof => "end of input".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub pos: usize,
}

pub fn tokenize(src: &str) -> Result<Vec<Token>> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < b.len() {
        let c = b[i];
        // whitespace
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // comment: -- to end of line, # is accepted too
        if c == '#' || (c == '-' && i + 1 < b.len() && b[i + 1] == '-') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        let tok = match c {
            '{' => {
                i += 1;
                Tok::LBrace
            }
            '}' => {
                i += 1;
                Tok::RBrace
            }
            '(' => {
                i += 1;
                Tok::LParen
            }
            ')' => {
                i += 1;
                Tok::RParen
            }
            '[' => {
                i += 1;
                Tok::LBracket
            }
            ']' => {
                i += 1;
                Tok::RBracket
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            ':' => {
                i += 1;
                Tok::Colon
            }
            '@' => {
                i += 1;
                Tok::At
            }
            '*' => {
                i += 1;
                Tok::Star
            }
            '~' => {
                i += 1;
                Tok::Tilde
            }
            ';' => {
                i += 1;
                continue; // statement separator, ignored
            }
            '=' => {
                i += 1;
                if i < b.len() && b[i] == '=' {
                    i += 1;
                }
                Tok::Eq
            }
            '!' => {
                i += 1;
                if i < b.len() && b[i] == '=' {
                    i += 1;
                    Tok::Ne
                } else {
                    return Err(Error::Query(format!(
                        "position {start}: `!` alone is invalid"
                    )));
                }
            }
            '<' => {
                i += 1;
                if i < b.len() && b[i] == '=' {
                    i += 1;
                    Tok::Le
                } else if i < b.len() && b[i] == '>' {
                    i += 1;
                    Tok::Ne
                } else {
                    Tok::Lt
                }
            }
            '>' => {
                i += 1;
                if i < b.len() && b[i] == '=' {
                    i += 1;
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            '$' => {
                i += 1;
                let s = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if s == i {
                    return Err(Error::Query(format!(
                        "position {start}: expected a number after `$`"
                    )));
                }
                let n: usize = b[s..i].iter().collect::<String>().parse().unwrap();
                if n == 0 {
                    return Err(Error::Query("parameters start at $1".into()));
                }
                Tok::Param(n - 1)
            }
            '"' | '\'' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= b.len() {
                        return Err(Error::Query(format!(
                            "position {start}: unterminated string"
                        )));
                    }
                    if b[i] == '\\' && i + 1 < b.len() {
                        i += 1;
                        s.push(match b[i] {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '0' => '\0',
                            other => other,
                        });
                        i += 1;
                        continue;
                    }
                    if b[i] == quote {
                        i += 1;
                        break;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                Tok::Str(s)
            }
            c if c.is_ascii_digit()
                || (c == '-' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) =>
            {
                let s = i;
                if b[i] == '-' {
                    i += 1;
                }
                let mut is_float = false;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == '_') {
                    i += 1;
                }
                if i < b.len() && b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                    is_float = true;
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < b.len() && (b[i] == 'e' || b[i] == 'E') {
                    is_float = true;
                    i += 1;
                    if i < b.len() && (b[i] == '+' || b[i] == '-') {
                        i += 1;
                    }
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text: String = b[s..i].iter().filter(|c| **c != '_').collect();
                if is_float {
                    Tok::Float(fenec_core::num::parse_f64(&text).ok_or_else(|| {
                        Error::Query(format!("position {s}: invalid decimal number `{text}`"))
                    })?)
                } else {
                    Tok::Int(text.parse().map_err(|_| {
                        Error::Query(format!("position {s}: invalid integer `{text}`"))
                    })?)
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let s = i;
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                    i += 1;
                }
                Tok::Ident(b[s..i].iter().collect())
            }
            other => {
                return Err(Error::Query(format!(
                    "position {start}: unexpected character `{other}`"
                )))
            }
        };
        out.push(Token { tok, pos: start });
    }
    out.push(Token {
        tok: Tok::Eof,
        pos: b.len(),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let t = tokenize(r#"get docs where year >= 2020 and title ~ "rust" limit 10"#).unwrap();
        assert!(matches!(t[0].tok, Tok::Ident(ref s) if s == "get"));
        assert!(t.iter().any(|t| t.tok == Tok::Ge));
        assert!(t.iter().any(|t| t.tok == Tok::Tilde));
    }

    #[test]
    fn numbers_and_params() {
        let t = tokenize("[-0.5, 1e-3, 42] $2").unwrap();
        assert_eq!(t[1].tok, Tok::Float(-0.5));
        assert_eq!(t[3].tok, Tok::Float(1e-3));
        assert_eq!(t[5].tok, Tok::Int(42));
        assert_eq!(t[7].tok, Tok::Param(1));
    }

    #[test]
    fn comments_skipped() {
        let t = tokenize("get docs -- comment\n# another comment\nlimit 1").unwrap();
        let idents: Vec<String> = t
            .iter()
            .filter_map(|x| match &x.tok {
                Tok::Ident(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(idents, vec!["get", "docs", "limit"]);
    }
}
