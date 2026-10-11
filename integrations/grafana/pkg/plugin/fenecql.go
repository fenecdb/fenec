package plugin

import (
	"fmt"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

// The tokens of a statement that binding and framing look at. The plugin
// reads a statement only to find where a name stands for a value -- `$sym`,
// `${n:int}`, never inside a string or a comment -- and a few keywords
// around it; the server parses it. So the walk follows fenec-ql's lexer
// (crates/fenec-ql/src/lexer.rs) only where the two must agree: what a
// string, a comment, a word and a `$` are.
type tokKind int

const (
	tokWord  tokKind = iota // a name or a keyword: `get`, `in`, `meta.source`
	tokParam                // `$3`
	tokRef                  // `$sym`, `${sym}`, `${sym:int}`: the plugin's
	tokOpen                 // `(` `[` `{`
	tokClose                // `)` `]` `}`
	tokComma
	tokOther // a string, a number, an operator
)

type token struct {
	kind       tokKind
	start, end int // its bytes in the text
	text       string
	n          int    // tokParam: the number
	name, as   string // tokRef: the name, and the type after `:`
	depth      int    // how many brackets it is inside
}

// scan splits a statement into tokens, or says where it cannot: a string
// left open, a `${` with no `}`, a `$` before nothing the server or the
// plugin reads. The server refuses each of them too; refused here, a name
// inside an open string is never taken for a value.
func scan(text string) ([]token, error) {
	var out []token
	depth := 0
	b := text
	i := 0
	for i < len(b) {
		r, size := utf8.DecodeRuneInString(b[i:])
		switch {
		case unicode.IsSpace(r):
			i += size
		case r == '#' || (r == '-' && i+1 < len(b) && b[i+1] == '-'):
			for i < len(b) && b[i] != '\n' {
				i++
			}
		case r == '"' || r == '\'':
			start := i
			quote := b[i]
			i++
			closed := false
			for i < len(b) {
				if b[i] == '\\' && i+1 < len(b) {
					i += 2
					continue
				}
				if b[i] == quote {
					i++
					closed = true
					break
				}
				i++
			}
			if !closed {
				return nil, fmt.Errorf("position %d: a string is not closed", start)
			}
			out = append(out, token{kind: tokOther, start: start, end: i, text: b[start:i], depth: depth})
		case r == '$':
			t, err := scanDollar(b, i)
			if err != nil {
				return nil, err
			}
			t.depth = depth
			out = append(out, t)
			i = t.end
		case unicode.IsLetter(r) || r == '_':
			start := i
			i += size
			for i < len(b) {
				c, n := utf8.DecodeRuneInString(b[i:])
				if unicode.IsLetter(c) || unicode.IsDigit(c) || c == '_' {
					i += n
					continue
				}
				// A path, `meta.source.rank`, is one word.
				if c == '.' && i+1 < len(b) {
					next, _ := utf8.DecodeRuneInString(b[i+1:])
					if unicode.IsLetter(next) || next == '_' {
						i++
						continue
					}
				}
				break
			}
			out = append(out, token{kind: tokWord, start: start, end: i, text: b[start:i], depth: depth})
		case r == '(' || r == '[' || r == '{':
			out = append(out, token{kind: tokOpen, start: i, end: i + 1, text: b[i : i+1], depth: depth})
			depth++
			i++
		case r == ')' || r == ']' || r == '}':
			if depth > 0 {
				depth--
			}
			out = append(out, token{kind: tokClose, start: i, end: i + 1, text: b[i : i+1], depth: depth})
			i++
		case r == ',':
			out = append(out, token{kind: tokComma, start: i, end: i + 1, text: ",", depth: depth})
			i++
		case unicode.IsDigit(r):
			// A number, `12_000`, `1.5e3`: its letters are not a word.
			start := i
			for i < len(b) && (isDigit(b[i]) || b[i] == '_' || b[i] == '.' || b[i] == 'e' || b[i] == 'E' ||
				((b[i] == '+' || b[i] == '-') && (b[i-1] == 'e' || b[i-1] == 'E'))) {
				i++
			}
			// A unit after a duration, `1h`, `30s`, stays the number's.
			for i < len(b) && isLetter(b[i]) {
				i++
			}
			out = append(out, token{kind: tokOther, start: start, end: i, text: b[start:i], depth: depth})
		default:
			out = append(out, token{kind: tokOther, start: i, end: i + size, text: b[i : i+size], depth: depth})
			i += size
		}
	}
	return out, nil
}

func isDigit(c byte) bool  { return c >= '0' && c <= '9' }
func isLetter(c byte) bool { return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' }
func isNameByte(c byte) bool {
	return isLetter(c) || isDigit(c) || c == '_'
}

// validName is a variable's name: Grafana's own names are of these
// characters, and a name of others would be read up to the first.
func validName(s string) bool {
	if s == "" || isDigit(s[0]) {
		return false
	}
	for i := 0; i < len(s); i++ {
		if !isNameByte(s[i]) {
			return false
		}
	}
	return true
}

// scanDollar reads what follows a `$`: a parameter's number, which the
// server binds, or a name, which the plugin binds to a parameter of its
// own.
func scanDollar(b string, i int) (token, error) {
	start := i
	i++
	switch {
	case i < len(b) && isDigit(b[i]):
		for i < len(b) && isDigit(b[i]) {
			i++
		}
		n, err := strconv.Atoi(b[start+1 : i])
		if err != nil || n == 0 {
			return token{}, fmt.Errorf("position %d: parameters are numbered from $1", start)
		}
		return token{kind: tokParam, start: start, end: i, text: b[start:i], n: n}, nil
	case i < len(b) && b[i] == '{':
		end := strings.IndexByte(b[i:], '}')
		if end < 0 {
			return token{}, fmt.Errorf("position %d: `${` is not closed with `}`", start)
		}
		inner := b[i+1 : i+end]
		name, as, _ := strings.Cut(inner, ":")
		if !validName(name) {
			return token{}, fmt.Errorf("position %d: `${%s}` names no variable: a name is letters, digits and `_`", start, inner)
		}
		return token{kind: tokRef, start: start, end: i + end + 1, text: b[start : i+end+1], name: name, as: as}, nil
	case i < len(b) && (isLetter(b[i]) || b[i] == '_'):
		for i < len(b) && isNameByte(b[i]) {
			i++
		}
		return token{kind: tokRef, start: start, end: i, text: b[start:i], name: b[start+1 : i]}, nil
	}
	return token{}, fmt.Errorf("position %d: `$` before neither a number nor a variable's name", start)
}

// Shape is what the plugin reads of a statement besides its values: the
// collection, the lookups whose rows nest inside each row, and the first
// key of its group -- for a time series, the time.
type Shape struct {
	Verb       string   // get, select or explain
	Collection string   // the collection the statement reads
	Lookups    []string // the collections `lookup` hangs below, in order
	GroupFirst string   // the first word after `group`: a column's name or `bucket`
}

func shapeOf(toks []token) Shape {
	var s Shape
	words := toks[:0:0]
	for _, t := range toks {
		if t.kind == tokWord {
			words = append(words, t)
		}
	}
	if len(words) == 0 {
		return s
	}
	s.Verb = strings.ToLower(words[0].text)
	first := 0
	if s.Verb == "explain" && len(words) > 1 {
		first = 1
	}
	// `get articles select ...`, or SQL's order, `select title from
	// articles`: the collection is the word after `from` where there is one
	// before the first clause, else the word after `get`.
	for i, t := range toks {
		if t.kind != tokWord || t.depth != 0 {
			continue
		}
		if strings.EqualFold(t.text, "from") && i+1 < len(toks) && toks[i+1].kind == tokWord {
			s.Collection = toks[i+1].text
			break
		}
		if isClause(t.text) {
			break
		}
	}
	if s.Collection == "" && len(words) > first+1 {
		s.Collection = words[first+1].text
	}
	for i, t := range toks {
		if t.kind != tokWord || t.depth != 0 || i+1 >= len(toks) {
			continue
		}
		switch strings.ToLower(t.text) {
		case "lookup":
			if toks[i+1].kind == tokWord {
				s.Lookups = append(s.Lookups, toks[i+1].text)
			}
		case "group":
			// The driving collection's group: one after a lookup is the
			// lookup's.
			if s.GroupFirst == "" && len(s.Lookups) == 0 && toks[i+1].kind == tokWord {
				s.GroupFirst = toks[i+1].text
			}
		}
	}
	return s
}

func isClause(w string) bool {
	switch strings.ToLower(w) {
	case "where", "near", "match", "order", "limit", "offset", "group", "having", "lookup", "facet", "count", "require":
		return true
	}
	return false
}
