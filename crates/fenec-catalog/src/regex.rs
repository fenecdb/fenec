//! The regular expressions catalog queries are written with -- `^(docs)$`,
//! `^pg_toast`, `^(doc.*)$` -- matched by backtracking. Enough of POSIX for
//! the patterns psql and JDBC generate: alternation, groups, classes, the
//! four quantifiers and anchors. Its own, for the rule that holds everywhere
//! else in fenecdb: no dependency.

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Char(char),
    Any,
    Start,
    End,
    Class(Vec<(char, char)>, bool),
    Group(Vec<Vec<Node>>),
    Repeat(Box<Node>, usize, usize),
}

pub struct Regex {
    alts: Vec<Vec<Node>>,
    insensitive: bool,
}

impl Regex {
    pub fn new(pattern: &str, insensitive: bool) -> Option<Regex> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut at = 0;
        let alts = alternation(&chars, &mut at)?;
        if at != chars.len() {
            return None;
        }
        Some(Regex { alts, insensitive })
    }

    /// Whether the pattern matches anywhere in `text`, as `~` asks.
    pub fn is_match(&self, text: &str) -> bool {
        let text: Vec<char> = if self.insensitive {
            text.chars().flat_map(char::to_lowercase).collect()
        } else {
            text.chars().collect()
        };
        (0..=text.len()).any(|start| {
            self.alts
                .iter()
                .any(|seq| matches(seq, &text, start, self.insensitive, &mut |_| true))
        })
    }
}

fn alternation(p: &[char], at: &mut usize) -> Option<Vec<Vec<Node>>> {
    let mut alts = vec![sequence(p, at)?];
    while *at < p.len() && p[*at] == '|' {
        *at += 1;
        alts.push(sequence(p, at)?);
    }
    Some(alts)
}

fn sequence(p: &[char], at: &mut usize) -> Option<Vec<Node>> {
    let mut seq = Vec::new();
    while *at < p.len() && p[*at] != '|' && p[*at] != ')' {
        let atom = match p[*at] {
            '^' => {
                *at += 1;
                Node::Start
            }
            '$' => {
                *at += 1;
                Node::End
            }
            '.' => {
                *at += 1;
                Node::Any
            }
            '(' => {
                *at += 1;
                // `(?:` is a group too.
                if p.get(*at) == Some(&'?') && p.get(*at + 1) == Some(&':') {
                    *at += 2;
                }
                let inner = alternation(p, at)?;
                if p.get(*at) != Some(&')') {
                    return None;
                }
                *at += 1;
                Node::Group(inner)
            }
            '[' => class(p, at)?,
            '\\' => {
                *at += 1;
                let c = *p.get(*at)?;
                *at += 1;
                escape(c)
            }
            c => {
                *at += 1;
                Node::Char(c)
            }
        };
        let atom = match p.get(*at) {
            Some('*') => Node::Repeat(Box::new(atom), 0, usize::MAX),
            Some('+') => Node::Repeat(Box::new(atom), 1, usize::MAX),
            Some('?') => Node::Repeat(Box::new(atom), 0, 1),
            _ => {
                seq.push(atom);
                continue;
            }
        };
        *at += 1;
        // A lazy `*?` matches the same set of strings; only where it stops
        // differs, and `~` asks only whether it matches.
        if p.get(*at) == Some(&'?') {
            *at += 1;
        }
        seq.push(atom);
    }
    Some(seq)
}

fn escape(c: char) -> Node {
    match c {
        'd' => Node::Class(vec![('0', '9')], false),
        'w' => Node::Class(vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')], false),
        's' => Node::Class(
            vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
            false,
        ),
        other => Node::Char(other),
    }
}

fn class(p: &[char], at: &mut usize) -> Option<Node> {
    *at += 1; // [
    let negated = p.get(*at) == Some(&'^');
    if negated {
        *at += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let c = *p.get(*at)?;
        if c == ']' && !first {
            *at += 1;
            return Some(Node::Class(ranges, negated));
        }
        first = false;
        *at += 1;
        let c = if c == '\\' {
            let e = *p.get(*at)?;
            *at += 1;
            e
        } else {
            c
        };
        if p.get(*at) == Some(&'-') && p.get(*at + 1).is_some_and(|n| *n != ']') {
            let hi = p[*at + 1];
            *at += 2;
            ranges.push((c, hi));
        } else {
            ranges.push((c, c));
        }
    }
}

/// Matches `seq` against `text` from `at`; `rest` is asked whether what
/// follows can match from where this ends, which is how a repetition gives
/// back what the rest of the pattern needs.
fn matches(
    seq: &[Node],
    text: &[char],
    at: usize,
    insensitive: bool,
    rest: &mut dyn FnMut(usize) -> bool,
) -> bool {
    let Some((node, tail)) = seq.split_first() else {
        return rest(at);
    };
    let mut then = |pos: usize| matches(tail, text, pos, insensitive, rest);
    one(node, text, at, insensitive, &mut then)
}

/// Every way `node` can match at `at`, each handed to `then`.
fn one(
    node: &Node,
    text: &[char],
    at: usize,
    insensitive: bool,
    then: &mut dyn FnMut(usize) -> bool,
) -> bool {
    let fold = |c: char| {
        if insensitive {
            c.to_lowercase().next().unwrap_or(c)
        } else {
            c
        }
    };
    match node {
        Node::Start => at == 0 && then(at),
        Node::End => at == text.len() && then(at),
        Node::Any => at < text.len() && then(at + 1),
        Node::Char(c) => at < text.len() && text[at] == fold(*c) && then(at + 1),
        Node::Class(ranges, negated) => {
            if at >= text.len() {
                return false;
            }
            let c = text[at];
            let inside = ranges
                .iter()
                .any(|(lo, hi)| (fold(*lo)..=fold(*hi)).contains(&c) || (*lo..=*hi).contains(&c));
            inside != *negated && then(at + 1)
        }
        Node::Group(alts) => alts
            .iter()
            .any(|seq| matches(seq, text, at, insensitive, then)),
        Node::Repeat(inner, min, max) => repeat(inner, text, at, insensitive, *min, *max, then),
    }
}

/// Greedy: the longest run first, giving back one at a time.
fn repeat(
    inner: &Node,
    text: &[char],
    at: usize,
    insensitive: bool,
    min: usize,
    max: usize,
    then: &mut dyn FnMut(usize) -> bool,
) -> bool {
    if max > 0 {
        let mut more = |pos: usize| {
            // An empty match cannot make progress; stop repeating there.
            pos != at
                && repeat(
                    inner,
                    text,
                    pos,
                    insensitive,
                    min.saturating_sub(1),
                    max - 1,
                    then,
                )
        };
        if one(inner, text, at, insensitive, &mut more) {
            return true;
        }
    }
    min == 0 && then(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        Regex::new(p, false).unwrap().is_match(t)
    }

    #[test]
    fn the_patterns_clients_send() {
        assert!(m("^(docs)$", "docs"));
        assert!(!m("^(docs)$", "docs2"));
        assert!(m("^(doc.*)$", "documents"));
        assert!(m("^pg_toast", "pg_toast_2619"));
        assert!(!m("^pg_toast", "public"));
        assert!(m("^pg_", "pg_catalog"));
        assert!(m("^(a|b)$", "b"));
        assert!(m("^[a-c]+x?$", "abcab"));
        assert!(!m("^[^a-c]+$", "xa"));
        assert!(m("oc", "docs"));
        assert!(m("^d\\.s$", "d.s"));
        assert!(Regex::new("^DOCS$", true).unwrap().is_match("docs"));
        assert!(m("^(.*)$", ""));
        assert!(Regex::new("(unclosed", false).is_none());
    }
}
