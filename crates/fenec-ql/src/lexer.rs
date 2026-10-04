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
    /// Numbers alone between brackets, read as the parser reads such a
    /// list: into `f32`s, a vector ([`tokenize_vectors`]).
    Vector(Vec<f32>),
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
    Plus,
    Minus,
    Slash,
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
            Tok::Vector(v) => format!("a vector of {}", v.len()),
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
            Tok::Plus => "`+`".into(),
            Tok::Minus => "`-`".into(),
            Tok::Slash => "`/`".into(),
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

/// Whether a `-` after `last` takes something away (`n - 1`, `n-1`)
/// rather than starting a negative number (`= -1`, `[1, -2]`): it does
/// after what ends a value -- a name, a literal, a parameter, a closing
/// bracket. After a keyword, which is a name too, `-1` is a `-` and a `1`,
/// which the parser folds back into the number (`where x > 0 and -1 < y`).
fn subtracts(last: Option<&Token>) -> bool {
    matches!(
        last.map(|t| &t.tok),
        Some(
            Tok::Ident(_)
                | Tok::Str(_)
                | Tok::Int(_)
                | Tok::Float(_)
                | Tok::Param(_)
                | Tok::Vector(_)
                | Tok::RParen
                | Tok::RBracket
        )
    )
}

pub fn tokenize(src: &str) -> Result<Vec<Token>> {
    lex::<false>(src)
}

/// [`tokenize`], with numbers alone between brackets read at once into
/// the vector the parser makes of them (`Parser::numbers_in_brackets`): a
/// token and a pass over the text for the whole list, where each number and
/// each comma was a token of its own, and each number's text read three
/// times -- for its end, for a `_`, for its value. A `put` of 1 000 rows
/// holding a 128-dim vector each took 5.4 ms to parse, 4.6 of it the lexer.
/// Not after `in`, whose list keeps its numbers as they are written: an
/// integer an integer and a decimal an `f64`. The browser module too, for
/// its memory rather than the time: a token a number, a `put` of 1 000
/// 768-dim rows as text took it from 58 to 130 MB, which it never gives
/// back, for 0.3 KB brotli.
pub fn tokenize_vectors(src: &str) -> Result<Vec<Token>> {
    lex::<true>(src)
}

/// At a `[` at byte `i`: the numbers alone up to the `]` that closes it,
/// each read as a token of its own reads it and made an `f32` as the parser
/// makes it, and where the `]` ends. `None` for anything else -- an empty
/// list, a trailing comma, a comment, a `_` in a number, one that does not
/// read -- which the tokens then read as they always did.
fn numbers_at(src: &str, b: &[u8], mut i: usize) -> Option<(Vec<f32>, usize)> {
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let space = |i: usize| matches!(b.get(i), Some(b' ' | b'\t' | b'\n' | b'\r'));
    let mut v = Vec::new();
    i += 1;
    loop {
        while space(i) {
            i += 1;
        }
        // Natively a number is read in the one pass that finds its end,
        // where Clinger's path takes it (`num::clinger`, the JSON reader's):
        // 3.5 -> 2.3 ms of the 1 000 rows' parse. The browser module goes
        // the general way, and carries no second reader.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(x) = fenec_core::num::clinger(b, &mut i) {
            v.push(x as f32);
            while space(i) {
                i += 1;
            }
            match b.get(i) {
                Some(b',') => {
                    i += 1;
                    continue;
                }
                Some(b']') => return Some((v, i + 1)),
                _ => return None,
            }
        }
        let s = i;
        i += (b.get(i) == Some(&b'-')) as usize;
        if !digit(i) {
            return None;
        }
        while digit(i) {
            i += 1;
        }
        let mut float = false;
        if b.get(i) == Some(&b'.') && digit(i + 1) {
            float = true;
            i += 1;
            while digit(i) {
                i += 1;
            }
        }
        if let Some(b'e' | b'E') = b.get(i) {
            float = true;
            i += 1;
            i += matches!(b.get(i), Some(b'+' | b'-')) as usize;
            while digit(i) {
                i += 1;
            }
        }
        let text = src.get(s..i)?;
        v.push(match float {
            true => fenec_core::num::parse_f64(text)? as f32,
            false => text.parse::<i64>().ok()? as f32,
        });
        while space(i) {
            i += 1;
        }
        match b.get(i) {
            Some(b',') => i += 1,
            Some(b']') => return Some((v, i + 1)),
            _ => return None,
        }
    }
}

/// One body natively for each way, the flag a constant in each; the browser
/// module keeps one, the flag read at run time: [`tokenize`] is there only
/// for a list a json field is given (`fenec_ql::parse_exact`), and a copy
/// of its own was 2.9 KB of the module.
fn lex<const VECTORS: bool>(src: &str) -> Result<Vec<Token>> {
    lex_with(src, VECTORS)
}

#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
fn lex_with(src: &str, vectors: bool) -> Result<Vec<Token>> {
    // Walked a byte at a time, a character read whole only where one
    // outside ASCII stands. Collected into a `Vec<char>` first, and each
    // number copied into a `String` of its own to parse, a query holding a
    // 128-dim vector took 16.9 us to parse. The text is sliced with `get`,
    // always at a character's edge: an index that can panic kept its
    // panic's formatting of a `char` in the browser module, 2.7 KB brotli.
    let b = src.as_bytes();
    let mut i = 0usize;
    // A token's position is its character's, as the errors count it: the
    // bytes stepped past are counted as they are left, each but a UTF-8
    // continuation byte.
    let (mut chars, mut counted) = (0usize, 0usize);
    // A token about every four bytes, but no more than 65 536 ahead: a
    // list of numbers is one token, and a `put` of 1 000 768-dim rows had
    // 39 MB set aside for 7 000 -- in the browser module, whose memory is
    // never given back, for good.
    let mut out = Vec::with_capacity((b.len() / 4 + 2).min(1 << 16));
    // The character at byte `i`, and its length in bytes.
    let at = |i: usize| -> (char, usize) {
        match b[i] {
            c if c < 0x80 => (c as char, 1),
            _ => {
                let c = src.get(i..).and_then(|t| t.chars().next()).unwrap_or('\0');
                (c, c.len_utf8())
            }
        }
    };
    // Braces open: a document's are the first, an object literal's inside.
    let mut braces = 0usize;
    let next_is = |i: usize, want: u8| b.get(i) == Some(&want);
    let digit_at = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);

    while i < b.len() {
        let (c, len) = at(i);
        // whitespace
        if c.is_whitespace() {
            i += len;
            continue;
        }
        // comment: -- to end of line, # is accepted too
        if c == '#' || (c == '-' && next_is(i + 1, b'-')) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        chars += b[counted..i].iter().filter(|&&x| x & 0xC0 != 0x80).count();
        counted = i;
        let start = chars;
        let tok = match c {
            '{' => {
                i += 1;
                braces += 1;
                Tok::LBrace
            }
            '}' => {
                i += 1;
                braces = braces.saturating_sub(1);
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
            // Not inside an object literal, two braces down: only a json
            // field holds one, and its numbers stay as they are written.
            '[' if vectors && braces < 2 => {
                let after_in = matches!(out.last(), Some(Token { tok: Tok::Ident(w), .. }) if w.eq_ignore_ascii_case("in"));
                match !after_in {
                    true => match numbers_at(src, b, i) {
                        Some((v, end)) => {
                            i = end;
                            Tok::Vector(v)
                        }
                        None => {
                            i += 1;
                            Tok::LBracket
                        }
                    },
                    false => {
                        i += 1;
                        Tok::LBracket
                    }
                }
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
            '+' => {
                i += 1;
                Tok::Plus
            }
            '/' => {
                i += 1;
                Tok::Slash
            }
            '-' if !digit_at(i + 1) || subtracts(out.last()) => {
                i += 1;
                Tok::Minus
            }
            ';' => {
                i += 1;
                continue; // statement separator, ignored
            }
            '=' => {
                i += 1;
                if next_is(i, b'=') {
                    i += 1;
                }
                Tok::Eq
            }
            '!' => {
                i += 1;
                if next_is(i, b'=') {
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
                if next_is(i, b'=') {
                    i += 1;
                    Tok::Le
                } else if next_is(i, b'>') {
                    i += 1;
                    Tok::Ne
                } else {
                    Tok::Lt
                }
            }
            '>' => {
                i += 1;
                if next_is(i, b'=') {
                    i += 1;
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            '$' => {
                i += 1;
                let s = i;
                while digit_at(i) {
                    i += 1;
                }
                if s == i {
                    return Err(Error::Query(format!(
                        "position {start}: expected a number after `$`"
                    )));
                }
                // Unwrapped, a number past `usize` was a panic.
                let n: usize = src.get(s..i).unwrap_or_default().parse().map_err(|_| {
                    Error::Query(format!(
                        "position {start}: no parameter is numbered that high"
                    ))
                })?;
                if n == 0 {
                    return Err(Error::Query("parameters start at $1".into()));
                }
                Tok::Param(n - 1)
            }
            '"' | '\'' => {
                let quote = b[i];
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= b.len() {
                        return Err(Error::Query(format!(
                            "position {start}: unterminated string"
                        )));
                    }
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                        let (e, elen) = at(i);
                        s.push(match e {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '0' => '\0',
                            other => other,
                        });
                        i += elen;
                        continue;
                    }
                    if b[i] == quote {
                        i += 1;
                        break;
                    }
                    // A run of plain characters at once, a backslash that
                    // ends the text among them.
                    let run = i;
                    while i < b.len() && b[i] != quote && !(b[i] == b'\\' && i + 1 < b.len()) {
                        i += 1;
                    }
                    s.push_str(src.get(run..i).unwrap_or_default());
                }
                Tok::Str(s)
            }
            c if c.is_ascii_digit() || (c == '-' && digit_at(i + 1)) => {
                let s = i;
                if b[i] == b'-' {
                    i += 1;
                }
                let mut is_float = false;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                    i += 1;
                }
                if next_is(i, b'.') && digit_at(i + 1) {
                    is_float = true;
                    i += 1;
                    while digit_at(i) {
                        i += 1;
                    }
                }
                if next_is(i, b'e') || next_is(i, b'E') {
                    is_float = true;
                    i += 1;
                    if next_is(i, b'+') || next_is(i, b'-') {
                        i += 1;
                    }
                    while digit_at(i) {
                        i += 1;
                    }
                }
                // Read where it stands: a copy only to drop `_`s.
                let raw = src.get(s..i).unwrap_or_default();
                let owned;
                let text = match raw.contains('_') {
                    true => {
                        owned = raw.replace('_', "");
                        owned.as_str()
                    }
                    false => raw,
                };
                if is_float {
                    Tok::Float(fenec_core::num::parse_f64(text).ok_or_else(|| {
                        Error::Query(format!("position {start}: invalid decimal number `{text}`"))
                    })?)
                } else {
                    Tok::Int(text.parse().map_err(|_| {
                        Error::Query(format!("position {start}: invalid integer `{text}`"))
                    })?)
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let s = i;
                i += len;
                loop {
                    while i < b.len() {
                        let (c, l) = at(i);
                        if !(c.is_alphanumeric() || c == '_') {
                            break;
                        }
                        i += l;
                    }
                    // `meta.source.rank`: a dot between two names is a path,
                    // one token, which the parser takes where a field goes.
                    // A dot was read only inside a number before, so no text
                    // that parsed reads differently.
                    let next = match b.get(i) {
                        Some(b'.') if i + 1 < b.len() => at(i + 1).0,
                        _ => break,
                    };
                    if !(next.is_alphabetic() || next == '_') {
                        break;
                    }
                    i += 1;
                }
                Tok::Ident(src.get(s..i).unwrap_or_default().to_string())
            }
            other => {
                return Err(Error::Query(format!(
                    "position {start}: unexpected character `{other}`"
                )))
            }
        };
        out.push(Token { tok, pos: start });
    }
    chars += b[counted..].iter().filter(|&&x| x & 0xC0 != 0x80).count();
    out.push(Token {
        tok: Tok::Eof,
        pos: chars,
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
    fn a_dot_between_names_is_one_path() {
        let t = tokenize("where meta.source.rank >= 2.5 and x.y_1 = a.").unwrap_err();
        assert!(t.to_string().contains('.'), "{t}");
        let t = tokenize("where meta.source.rank >= 2.5 and x.y_1 = a").unwrap();
        assert_eq!(t[1].tok, Tok::Ident("meta.source.rank".into()));
        assert_eq!(t[3].tok, Tok::Float(2.5));
        assert_eq!(t[5].tok, Tok::Ident("x.y_1".into()));
        // Not before a digit: `a.5` is no path.
        assert!(tokenize("a.5").is_err());
    }

    /// A `-` after what ends a value takes away; anywhere else it is a
    /// number's sign, as it always was.
    #[test]
    fn a_minus_after_a_value_subtracts() {
        let toks =
            |s: &str| -> Vec<Tok> { tokenize(s).unwrap().into_iter().map(|t| t.tok).collect() };
        let n = || Tok::Ident("n".into());
        assert_eq!(toks("n-1"), [n(), Tok::Minus, Tok::Int(1), Tok::Eof]);
        assert_eq!(toks("n - 1"), [n(), Tok::Minus, Tok::Int(1), Tok::Eof]);
        assert_eq!(
            toks("$1-2"),
            [Tok::Param(0), Tok::Minus, Tok::Int(2), Tok::Eof]
        );
        assert_eq!(
            toks("(n)-2.5")[3..],
            [Tok::Minus, Tok::Float(2.5), Tok::Eof]
        );
        assert_eq!(toks("n = -1"), [n(), Tok::Eq, Tok::Int(-1), Tok::Eof]);
        assert_eq!(
            toks("[1, -2]")[3..],
            [Tok::Int(-2), Tok::RBracket, Tok::Eof]
        );
        assert_eq!(toks("-n"), [Tok::Minus, n(), Tok::Eof]);
        assert_eq!(
            toks("n+1*2/3")[1..6],
            [Tok::Plus, Tok::Int(1), Tok::Star, Tok::Int(2), Tok::Slash]
        );
        // A comment is still a comment.
        assert_eq!(toks("n -- 1"), [n(), Tok::Eof]);
    }

    #[test]
    fn numbers_and_params() {
        let t = tokenize("[-0.5, 1e-3, 42] $2").unwrap();
        assert_eq!(t[1].tok, Tok::Float(-0.5));
        assert_eq!(t[3].tok, Tok::Float(1e-3));
        assert_eq!(t[5].tok, Tok::Int(42));
        assert_eq!(t[7].tok, Tok::Param(1));
    }

    /// The tokenizer as it was, over a `Vec<char>`: what the byte walk is
    /// held to.
    fn tokenize_chars(src: &str) -> Result<Vec<Token>> {
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
                '+' => {
                    i += 1;
                    Tok::Plus
                }
                '/' => {
                    i += 1;
                    Tok::Slash
                }
                '-' if !(i + 1 < b.len() && b[i + 1].is_ascii_digit()) || subtracts(out.last()) => {
                    i += 1;
                    Tok::Minus
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
                    loop {
                        while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                            i += 1;
                        }
                        // A path, `meta.lang`, since paths were read.
                        match (b.get(i), b.get(i + 1)) {
                            (Some('.'), Some(&n)) if n.is_alphabetic() || n == '_' => i += 1,
                            _ => break,
                        }
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

    /// Generated texts of every kind of token, whitespace, comment and
    /// mistake, ASCII and not: the byte walk gives the tokens, positions
    /// and errors the character walk gave.
    #[test]
    fn the_byte_walk_reads_as_the_char_walk_did() {
        let pieces = [
            "get",
            "docs",
            "şehir",
            "名前",
            "_x1",
            "a_b",
            "Ω",
            " ",
            "  ",
            "\t",
            "\n",
            "\u{0B}",
            "\u{A0}",
            "\u{3000}",
            "\u{2028}",
            "{",
            "}",
            "(",
            ")",
            "[",
            "]",
            ",",
            ":",
            "@",
            "*",
            "~",
            ";",
            "=",
            "==",
            "!=",
            "!",
            "<",
            "<=",
            "<>",
            ">",
            ">=",
            "$1",
            "$23",
            "$",
            "$0",
            "12",
            "-3",
            "-3.5",
            "1_000",
            "1e5",
            "2.5E-3",
            "7e",
            "1.",
            "-",
            "+",
            "/",
            "n-1",
            "-x",
            "--c\n",
            "-- c",
            "#c\n",
            "#",
            "\"s\"",
            "'q'",
            "\"a\\\"b\"",
            "'x\\n'",
            "\"ü\\ğ\"",
            "\"unterminated",
            "'",
            "\\",
            "\"a\\",
            "é",
            "\u{301}",
            "𝔸",
            "%",
            "^",
            "&",
            "|",
            "+",
            "/",
            "?",
            ".",
            "0.5",
            "-0",
            "99999999999999999999",
            "1e400",
            "_",
            "x",
            "\u{0}",
        ];
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..40_000 {
            let mut text = String::new();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            for _ in 0..(x % 12) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                text.push_str(pieces[(x % pieces.len() as u64) as usize]);
            }
            // The character walk unwrapped a parameter's number, and a
            // number past `usize` was a panic; the byte walk refuses it.
            let b = match std::panic::catch_unwind(|| tokenize_chars(&text)) {
                Ok(b) => b,
                Err(_) => {
                    assert!(tokenize(&text).is_err(), "{text:?}");
                    continue;
                }
            };
            let a = tokenize(&text);
            match (&a, &b) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{text:?}"),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{text:?}"),
                _ => panic!("{text:?}: {a:?} against {b:?}"),
            }
        }
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
