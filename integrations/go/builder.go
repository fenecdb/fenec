package fenecdb

// The query builder: FenecQL text and its parameters from a chain of calls.
//
//	rows, err := db.From("docs").
//		Select("title").
//		Where("year", ">=", 2024).
//		Near("embed", vec, fenecdb.Ef(64)).
//		Limit(5).
//		Rows(ctx)
//
// It makes the text web/fenec.js's builder makes of the same chain, to the
// byte, and so do the Python and .NET builders: integrations/builder-golden.json
// holds the chains and what each must make, and every builder's tests run
// it. Every value goes in as a parameter; a name -- a collection, a field,
// a path into a json field -- cannot, so names are checked against
// FenecQL's own rule, and that check is the injection boundary.
//
// A *Builder is immutable: each call hands back a copy, so a base query can
// be kept and branched from, from several goroutines too. A step that is
// refused leaves its error on the copy, every later step passes it on, and
// the first one comes back from whatever ends the chain -- as the JS
// builder throws at the first.

import (
	"context"
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"sort"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

// QueryError is a statement the builder refused before sending anything:
// a name that is not one, an operator it does not know, clauses that do
// not go together. Message is the JS builder's, word for word.
type QueryError struct{ Message string }

func (e *QueryError) Error() string { return "fenecdb: " + e.Message }

func refuse(format string, args ...any) error {
	return &QueryError{Message: fmt.Sprintf(format, args...)}
}

// MaxLookupDepth is how many lookups a chain holds; the engine refuses more.
const MaxLookupDepth = 8

// Operator names, the symbols and the words for them.
var ops = map[string]string{
	"=": "=", "eq": "=",
	"!=": "!=", "ne": "!=", "neq": "!=",
	"<": "<", "lt": "<",
	"<=": "<=", "lte": "<=", "le": "<=",
	">": ">", "gt": ">",
	">=": ">=", "gte": ">=", "ge": ">=",
	"~": "~", "like": "~", "contains": "~",
	"has": "has",
	"in":  "in",
}

// FenecQL's identifier: a Unicode letter or _, then letters, digits and _,
// as the lexer reads it -- JavaScript's \p{Alphabetic} and \p{N}, which
// holds the marks of Other_Alphabetic, a Devanagari vowel sign among them.
func identStart(r rune) bool {
	return r == '_' || unicode.IsLetter(r) || unicode.Is(unicode.Nl, r) || unicode.Is(unicode.Other_Alphabetic, r)
}

func isIdent(s string) bool {
	if s == "" || !utf8.ValidString(s) {
		return false
	}
	for i, r := range s {
		if !(identStart(r) || (i > 0 && unicode.IsNumber(r))) {
			return false
		}
	}
	return true
}

func ident(name, what string) (string, error) {
	if !isIdent(name) {
		return "", refuse("invalid %s name: %s", what, jsQuote(name))
	}
	return name, nil
}

// fieldPath is a field's name, or a path into a json field: meta.lang.
func fieldPath(name string) (string, error) {
	for _, p := range strings.Split(name, ".") {
		if !isIdent(p) {
			return "", refuse("invalid field name: %s", jsQuote(name))
		}
	}
	return name, nil
}

// jsQuote writes a name as JSON.stringify does, which the messages quote
// with: encoding/json would escape <, > and & and the line separators.
func jsQuote(s string) string {
	var b strings.Builder
	b.WriteByte('"')
	for _, r := range s {
		switch r {
		case '"':
			b.WriteString(`\"`)
		case '\\':
			b.WriteString(`\\`)
		case '\b':
			b.WriteString(`\b`)
		case '\f':
			b.WriteString(`\f`)
		case '\n':
			b.WriteString(`\n`)
		case '\r':
			b.WriteString(`\r`)
		case '\t':
			b.WriteString(`\t`)
		default:
			if r < 0x20 {
				fmt.Fprintf(&b, `\u%04x`, r)
			} else {
				b.WriteRune(r)
			}
		}
	}
	b.WriteByte('"')
	return b.String()
}

// What JavaScript's String.prototype.trim takes off, which the JS builder
// trims an aggregate with.
func jsSpace(r rune) bool {
	switch r {
	case '\t', '\n', '\v', '\f', '\r', ' ', 0xa0, 0x1680, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000, 0xfeff:
		return true
	}
	return r >= 0x2000 && r <= 0x200a
}

func asciiIdent(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		letter := c == '_' || (c|0x20 >= 'a' && c|0x20 <= 'z')
		if !(letter || (i > 0 && c >= '0' && c <= '9')) {
			return false
		}
	}
	return true
}

// column is a select item: a field, or an aggregate spelled as FenecQL
// spells it -- count(*), sum(total), avg(f), min(f), max(f) -- answering
// under that name. Read by hand rather than by a case-blind pattern, which
// folds the Kelvin sign onto k where the JS builder's does not.
func column(name string) (string, bool, error) {
	s := strings.TrimFunc(name, jsSpace)
	if open := strings.IndexByte(s, '('); open > 0 && strings.HasSuffix(s, ")") {
		fn, arg := s[:open], s[open+1:len(s)-1]
		low := ""
		if asciiIdent(fn) && !strings.ContainsAny(fn, "_0123456789") {
			low = strings.ToLower(fn)
		}
		switch {
		case low == "count" && (arg == "" || arg == "*"):
			return "count(*)", true, nil
		case (low == "sum" || low == "avg" || low == "min" || low == "max") && asciiIdent(arg):
			return low + "(" + arg + ")", true, nil
		}
	}
	p, err := fieldPath(name)
	return p, false, err
}

func direction(dir string) (bool, error) {
	switch strings.ToLower(dir) {
	case "asc":
		return true, nil
	case "desc":
		return false, nil
	}
	return false, refuse("order direction must be 'asc' or 'desc': %s", dir)
}

// The collations the engine knows. The name is spliced into the text, so it
// is checked against the list rather than the name pattern.
func collation(name *string) (string, error) {
	if name == nil {
		return "", nil
	}
	if *name != "und" && *name != "tr" {
		return "", refuse("unknown collation: %s; there are 'und' and 'tr'", jsQuote(*name))
	}
	return *name, nil
}

// whole: limit, offset, ef and the rest are literals in FenecQL, never
// parameters -- a whole number JavaScript holds exactly.
func whole(n int, what string) (int, error) {
	if n < 0 || int64(n) > 1<<53-1 {
		return 0, refuse("%s must be a non-negative integer: %d", what, n)
	}
	return n, nil
}

// normalize is a value as it goes into the parameters: a time.Time as the
// text JavaScript's toISOString writes, which the JS builder sends a Date
// as -- UTC, to the millisecond -- inside lists and objects too.
func normalize(v any) any {
	switch v := v.(type) {
	case time.Time:
		return v.UTC().Format("2006-01-02T15:04:05.000Z")
	case []any:
		out := make([]any, len(v))
		for i, x := range v {
			out[i] = normalize(x)
		}
		return out
	case map[string]any:
		out := make(map[string]any, len(v))
		for k, x := range v {
			out[k] = normalize(x)
		}
		return out
	case Doc:
		out := make(map[string]any, len(v))
		for _, f := range v {
			out[f.Name] = normalize(f.Value)
		}
		return out
	}
	return v
}

// ------------------------------------------------------------ conditions

type node struct {
	t       string // and, or, not, null, in, cmp, raw
	items   []*node
	field   string
	op      string
	value   any
	values  []any
	negated bool
	sql     string
	params  []any
}

// Cond is a condition: what Or, And, Not, Raw, Cmp and Fields make. A
// refused one carries its error to the step that takes it.
type Cond struct {
	n   *node
	err error
}

// OpMap is a field's operators, in the order given: Ops("gte", 2020,
// "lt", 2030). "not" takes another OpMap, or nil for "is not null".
type OpMap struct {
	pairs []any
}

// Ops is an operator map for Fields: names and values in turn. A Go map
// would lose the order, and the order is the text's.
func Ops(pairs ...any) OpMap { return OpMap{pairs: pairs} }

// Or joins conditions with or.
func Or(conds ...Cond) Cond { return junction("or", conds) }

// And joins conditions with and: Where already ands, so this is only
// needed inside Or.
func And(conds ...Cond) Cond { return junction("and", conds) }

func junction(t string, conds []Cond) Cond {
	n := &node{t: t}
	for _, c := range conds {
		if c.err != nil {
			return Cond{err: c.err}
		}
		n.items = append(n.items, c.node())
	}
	return Cond{n: n}
}

// node is the condition's tree; the zero Cond is an empty and.
func (c Cond) node() *node {
	if c.n == nil {
		return &node{t: "and"}
	}
	return c.n
}

// Not negates a condition.
func Not(c Cond) Cond {
	if c.err != nil {
		return c
	}
	return Cond{n: &node{t: "not", items: []*node{c.node()}}}
}

// Raw is everything the builder cannot express (a function call): each ?
// is bound to the next parameter -- a literal ? goes in as one too.
//
//	q.WhereCond(fenecdb.Raw("cosine(embed, ?) > ?", vec, 0.5))
func Raw(sql string, params ...any) Cond {
	return Cond{n: &node{t: "raw", sql: sql, params: params}}
}

// Cmp is one comparison, Where's three arguments as a condition: for Or.
func Cmp(field, op string, value any) Cond {
	o, ok := ops[op]
	if !ok {
		return Cond{err: refuse("unknown operator `%s`", op)}
	}
	f, err := fieldPath(field)
	if err != nil {
		return Cond{err: err}
	}
	if o == "in" {
		return inCond(f, value)
	}
	return cmp(f, o, value)
}

// Fields is the JS builder's object condition -- names and specs in turn,
// joined with and. A spec is a value (equality), nil (is null), an OpMap,
// or a map[string]any read as one in the order of its sorted keys.
//
//	fenecdb.Fields("year", fenecdb.Ops("gte", 2024), "tags", fenecdb.Ops("has", "rust"))
func Fields(pairs ...any) Cond {
	if len(pairs)%2 != 0 {
		return Cond{err: refuse("Fields takes names and specs in pairs")}
	}
	var items []*node
	for i := 0; i < len(pairs); i += 2 {
		name, ok := pairs[i].(string)
		if !ok {
			return Cond{err: refuse("invalid field name: %v", pairs[i])}
		}
		f, err := fieldPath(name)
		if err != nil {
			return Cond{err: err}
		}
		c := fieldCond(f, pairs[i+1])
		if c.err != nil {
			return c
		}
		items = append(items, c.n)
	}
	if len(items) == 1 {
		return Cond{n: items[0]}
	}
	return Cond{n: &node{t: "and", items: items}}
}

func fieldCond(field string, spec any) Cond {
	var pairs []any
	switch s := spec.(type) {
	case nil:
		return Cond{n: &node{t: "null", field: field}}
	case OpMap:
		pairs = s.pairs
	case map[string]any:
		keys := make([]string, 0, len(s))
		for k := range s {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			pairs = append(pairs, k, s[k])
		}
	default:
		return cmp(field, "=", spec)
	}
	if len(pairs)%2 != 0 {
		return Cond{err: refuse("Ops takes operators and values in pairs (field: %s)", field)}
	}
	var items []*node
	for i := 0; i < len(pairs); i += 2 {
		k := fmt.Sprint(pairs[i])
		v := pairs[i+1]
		var c Cond
		if k == "not" {
			if v == nil {
				c = Cond{n: &node{t: "null", field: field, negated: true}}
			} else if c = fieldCond(field, v); c.err == nil {
				c = Cond{n: &node{t: "not", items: []*node{c.n}}}
			}
		} else if op, ok := ops[k]; !ok {
			return Cond{err: refuse("unknown operator `%s` (field: %s)", k, field)}
		} else if op == "in" {
			c = inCond(field, v)
		} else {
			c = cmp(field, op, v)
		}
		if c.err != nil {
			return c
		}
		items = append(items, c.n)
	}
	switch len(items) {
	case 0:
		return Cond{err: refuse("empty condition object (field: %s)", field)}
	case 1:
		return Cond{n: items[0]}
	}
	return Cond{n: &node{t: "and", items: items}}
}

func inCond(field string, values any) Cond {
	rv := reflect.ValueOf(values)
	if values == nil || (rv.Kind() != reflect.Slice && rv.Kind() != reflect.Array) {
		return Cond{err: refuse("`in` expects an array (field: %s)", field)}
	}
	if rv.Len() == 0 {
		return Cond{err: refuse("`in` does not accept an empty array (field: %s)", field)}
	}
	list := make([]any, rv.Len())
	for i := range list {
		list[i] = rv.Index(i).Interface()
	}
	return Cond{n: &node{t: "in", field: field, values: list}}
}

// = null is never true in FenecQL; what is meant is is null.
func cmp(field, op string, value any) Cond {
	if value == nil {
		switch op {
		case "=":
			return Cond{n: &node{t: "null", field: field}}
		case "!=":
			return Cond{n: &node{t: "null", field: field, negated: true}}
		}
		return Cond{err: refuse("`%s` cannot be used with null (field: %s)", op, field)}
	}
	return Cond{n: &node{t: "cmp", field: field, op: op, value: value}}
}

// prune flattens empty and single-child junctions before rendering: the
// parentheses depend on the child count, and rendering binds parameters.
func prune(c *node) *node {
	switch c.t {
	case "and", "or":
		var items []*node
		for _, x := range c.items {
			if p := prune(x); p != nil {
				items = append(items, p)
			}
		}
		switch len(items) {
		case 0:
			return nil
		case 1:
			return items[0]
		}
		return &node{t: c.t, items: items}
	case "not":
		if p := prune(c.items[0]); p != nil {
			return &node{t: "not", items: []*node{p}}
		}
		return nil
	}
	return c
}

type binder struct{ params []any }

func (b *binder) bind(v any) string {
	b.params = append(b.params, normalize(v))
	return fmt.Sprintf("$%d", len(b.params))
}

func render(c *node, b *binder, parent string) (string, error) {
	switch c.t {
	case "and", "or":
		parts := make([]string, len(c.items))
		for i, x := range c.items {
			s, err := render(x, b, c.t)
			if err != nil {
				return "", err
			}
			parts[i] = s
		}
		s := strings.Join(parts, " "+c.t+" ")
		// and binds tighter than or: one inside the other needs parens.
		if parent != "" && parent != c.t {
			s = "(" + s + ")"
		}
		return s, nil
	case "not":
		s, err := render(c.items[0], b, "")
		return "not (" + s + ")", err
	case "null":
		if c.negated {
			return c.field + " is not null", nil
		}
		return c.field + " is null", nil
	case "in":
		parts := make([]string, len(c.values))
		for i, v := range c.values {
			parts[i] = b.bind(v)
		}
		return c.field + " in [" + strings.Join(parts, ", ") + "]", nil
	case "cmp":
		return c.field + " " + c.op + " " + b.bind(c.value), nil
	}
	pieces := strings.Split(c.sql, "?")
	var out strings.Builder
	for i, p := range pieces[:len(pieces)-1] {
		if i >= len(c.params) {
			return "", refuse("raw(): more `?` placeholders than parameters")
		}
		out.WriteString(p)
		out.WriteString(b.bind(c.params[i]))
	}
	if len(pieces)-1 != len(c.params) {
		return "", refuse("raw(): too many parameters given")
	}
	out.WriteString(pieces[len(pieces)-1])
	return out.String(), nil
}

// --------------------------------------------------------------- options

// Opt is an option of a builder step: Ef and Exact for Near, K and
// Candidates for Fuse and Rerank, Collate for Order and Sort, All for
// Update and Delete, and On, ParentKey, Select, Where, Required, Sort,
// Limit and Offset for Lookup. A step reads the ones it has and passes
// over the rest, as the JS builder's option objects do.
type Opt func(*opts)

type opts struct {
	ef, k, candidates, limit, offset *int
	exact, required, all             bool
	on, parentKey, collate           *string
	sel                              []string
	selSet                           bool
	where                            *Cond
	sort                             []sortOpt
}

type sortOpt struct {
	field, dir string
	collate    *string
}

func gather(list []Opt) opts {
	var o opts
	for _, f := range list {
		if f != nil {
			f(&o)
		}
	}
	return o
}

func intp(n int) *int { return &n }

// The options, each named for the clause word it writes or the JS
// builder's option of that name.
func strp(s string) *string      { return &s }
func Ef(n int) Opt               { return func(o *opts) { o.ef = intp(n) } }
func Exact() Opt                 { return func(o *opts) { o.exact = true } }
func K(n int) Opt                { return func(o *opts) { o.k = intp(n) } }
func Candidates(n int) Opt       { return func(o *opts) { o.candidates = intp(n) } }
func Collate(name string) Opt    { return func(o *opts) { o.collate = strp(name) } }
func All() Opt                   { return func(o *opts) { o.all = true } }
func On(field string) Opt        { return func(o *opts) { o.on = strp(field) } }
func ParentKey(field string) Opt { return func(o *opts) { o.parentKey = strp(field) } }
func Required() Opt              { return func(o *opts) { o.required = true } }
func Limit(n int) Opt            { return func(o *opts) { o.limit = intp(n) } }
func Offset(n int) Opt           { return func(o *opts) { o.offset = intp(n) } }

// Select names a lookup's fields; "*" or none is every field.
func Select(cols ...string) Opt {
	return func(o *opts) { o.sel, o.selSet = cols, true }
}

// Where is a lookup's filter, over the looked-up collection.
func Where(c Cond) Opt { return func(o *opts) { o.where = &c } }

// Sort adds a key to a lookup's order, as Order does to the query's.
func Sort(field, dir string, options ...Opt) Opt {
	c := gather(options).collate
	return func(o *opts) { o.sort = append(o.sort, sortOpt{field, dir, c}) }
}

// ---------------------------------------------------------------- the query

type sortKey struct {
	field   string
	asc     bool
	collate string
}

func (k sortKey) String() string {
	s := k.field
	if k.collate != "" {
		s += " collate " + k.collate
	}
	if k.asc {
		return s + " asc"
	}
	return s + " desc"
}

type lookupLevel struct {
	collection, on, parent string
	project                []string
	cond                   []*node
	required               bool
	order                  []sortKey
	limit                  int
	hasLimit               bool
	offset                 int
}

type vectorClause struct {
	field  string
	vector any
	n      int // ef, or a rerank's candidates; -1 when not given
	exact  bool
}

// Builder is a query over one collection, made by Client.From or From.
type Builder struct {
	client     *Client
	err        error
	collection string
	project    []string
	aggregate  bool
	group      string
	cond       []*node
	near       *vectorClause
	match      *[2]any
	rerank     *vectorClause
	fuse       *[2]int // k, candidates; -1 when not given
	order      []sortKey
	limit      int
	hasLimit   bool
	offset     int
	count      bool
	lookups    []lookupLevel
}

// From is the query builder over a collection, bound to no client: for its
// text alone (ToFenecQL).
func From(collection string) *Builder {
	b := &Builder{}
	b.collection, b.err = ident(collection, "collection")
	return b
}

// From is the query builder over a collection: chain Select, Where, Near,
// Order, Limit ... and end with Rows, First, Count, or a write -- Insert,
// Update, Delete.
func (c *Client) From(collection string) *Builder {
	b := From(collection)
	b.client = c
	return b
}

// step hands back a copy with change applied, or the copy holding the
// first error. The slices are clipped, so an append never reaches into
// another copy's.
func (b *Builder) step(change func(*Builder) error) *Builder {
	d := *b
	d.project = slices.Clip(d.project)
	d.cond = slices.Clip(d.cond)
	d.order = slices.Clip(d.order)
	d.lookups = slices.Clip(d.lookups)
	if d.err == nil {
		d.err = change(&d)
	}
	return &d
}

// Err is the first step that was refused, or nil.
func (b *Builder) Err() error { return b.err }

// Collection is the collection the query is over.
func (b *Builder) Collection() string { return b.collection }

// Select names the fields; none, or "*", is every field. Aggregates go in
// the same list as FenecQL spells them and answer under that name:
// Select("status", "count(*)", "sum(total)").Group("status").
func (b *Builder) Select(cols ...string) *Builder {
	return b.step(func(d *Builder) error {
		if len(cols) == 0 || slices.Contains(cols, "*") {
			d.project, d.aggregate = nil, false
			return nil
		}
		project, aggregate := make([]string, len(cols)), false
		for i, c := range cols {
			t, agg, err := column(c)
			if err != nil {
				return err
			}
			project[i], aggregate = t, aggregate || agg
		}
		d.project, d.aggregate = project, aggregate
		return nil
	})
}

// Group makes a row per value of field, for a select list that aggregates.
func (b *Builder) Group(field string) *Builder {
	return b.step(func(d *Builder) (err error) {
		d.group, err = ident(field, "field")
		return err
	})
}

// Where adds `field op value`; successive conditions join with and. The op
// is a symbol or its word: =, !=, <, <=, >, >=, ~, has, in (a slice), or
// eq, ne, lt, gte, like, contains ... A nil value with = or != is is null
// and is not null.
func (b *Builder) Where(field, op string, value any) *Builder {
	return b.WhereCond(Cmp(field, op, value))
}

// WhereCond adds a condition that Or, And, Not, Raw or Fields made.
func (b *Builder) WhereCond(c Cond) *Builder {
	return b.step(func(d *Builder) error {
		if c.err != nil {
			return c.err
		}
		d.cond = append(d.cond, c.node())
		return nil
	})
}

// OrWhere joins everything conditioned so far to `field op value` with or.
func (b *Builder) OrWhere(field, op string, value any) *Builder {
	return b.OrWhereCond(Cmp(field, op, value))
}

// OrWhereCond joins everything conditioned so far to c with or.
func (b *Builder) OrWhereCond(c Cond) *Builder {
	return b.step(func(d *Builder) error {
		if c.err != nil {
			return c.err
		}
		if len(d.cond) == 0 {
			d.cond = []*node{c.node()}
			return nil
		}
		left := &node{t: "and", items: d.cond}
		d.cond = []*node{{t: "or", items: []*node{left, c.node()}}}
		return nil
	})
}

// Near is `near field $n [ef N] [exact]`: options Ef and Exact.
func (b *Builder) Near(field string, vector any, options ...Opt) *Builder {
	return b.step(func(d *Builder) error {
		f, err := ident(field, "field")
		if err != nil {
			return err
		}
		o := gather(options)
		v := &vectorClause{field: f, vector: vector, n: -1, exact: o.exact}
		if o.ef != nil {
			if v.n, err = whole(*o.ef, "ef"); err != nil {
				return err
			}
		}
		d.near = v
		return nil
	})
}

// Match is `match field $n`: BM25 over a @text index.
func (b *Builder) Match(field, query string) *Builder {
	return b.step(func(d *Builder) error {
		f, err := ident(field, "field")
		d.match = &[2]any{f, query}
		return err
	})
}

// Fuse is `fuse [k N] [candidates N]`: with both Match and Near, ranks by
// both, a document scoring 1 / (k + rank) from each list it is on.
// Options K and Candidates.
func (b *Builder) Fuse(options ...Opt) *Builder {
	return b.step(func(d *Builder) error {
		o, f := gather(options), [2]int{-1, -1}
		var err error
		if o.k != nil {
			if f[0], err = whole(*o.k, "k"); err != nil {
				return err
			}
		}
		if o.candidates != nil {
			if f[1], err = whole(*o.candidates, "candidates"); err != nil {
				return err
			}
		}
		d.fuse = &f
		return nil
	})
}

// Rerank is `rerank field $n [candidates N]`: reorders what Match found by
// exact distance, the vectors read out of the store. Option Candidates.
func (b *Builder) Rerank(field string, vector any, options ...Opt) *Builder {
	return b.step(func(d *Builder) error {
		f, err := ident(field, "field")
		if err != nil {
			return err
		}
		o := gather(options)
		v := &vectorClause{field: f, vector: vector, n: -1}
		if o.candidates != nil {
			if v.n, err = whole(*o.candidates, "candidates"); err != nil {
				return err
			}
		}
		d.rerank = v
		return nil
	})
}

// Lookup is `lookup name on child [= parent] ...`: each row's children,
// attached to it. On names the child's field (needed), ParentKey the
// parent's (id unless given); Select, Where, Sort, Limit and Offset bind
// to the looked-up collection, and Limit counts children per parent.
// Required drops a parent no child matches. Called again, it chains onto
// the collection the call before named.
func (b *Builder) Lookup(name string, options ...Opt) *Builder {
	return b.step(func(d *Builder) (err error) {
		o := gather(options)
		if o.on == nil || *o.on == "" {
			return refuse("lookup needs `on`: the child field holding the key")
		}
		l := lookupLevel{required: o.required}
		if l.collection, err = ident(name, "collection"); err != nil {
			return err
		}
		if l.on, err = ident(*o.on, "field"); err != nil {
			return err
		}
		if o.parentKey != nil {
			if l.parent, err = ident(*o.parentKey, "field"); err != nil {
				return err
			}
		}
		if o.selSet && !slices.Contains(o.sel, "*") {
			for _, c := range o.sel {
				p, err := fieldPath(c)
				if err != nil {
					return err
				}
				l.project = append(l.project, p)
			}
		}
		if o.where != nil {
			if o.where.err != nil {
				return o.where.err
			}
			l.cond = []*node{o.where.node()}
		}
		for _, s := range o.sort {
			k, err := sortKeyOf(s.field, s.dir, s.collate, false)
			if err != nil {
				return err
			}
			l.order = append(l.order, k)
		}
		if o.limit != nil {
			if l.limit, err = whole(*o.limit, "limit"); err != nil {
				return err
			}
			l.hasLimit = true
		}
		if o.offset != nil {
			if l.offset, err = whole(*o.offset, "offset"); err != nil {
				return err
			}
		}
		d.lookups = append(d.lookups, l)
		return nil
	})
}

func sortKeyOf(field, dir string, collate *string, aggregates bool) (sortKey, error) {
	var k sortKey
	var err error
	if aggregates {
		// Over groups a key may be an aggregate of the list, by its name.
		k.field, _, err = column(field)
	} else {
		k.field, err = fieldPath(field)
	}
	if err != nil {
		return k, err
	}
	if k.asc, err = direction(dir); err != nil {
		return k, err
	}
	k.collate, err = collation(collate)
	return k, err
}

// Order adds a key: when the keys before it tie, it decides. The direction
// is "asc" or "desc"; Collate("tr") puts text in Turkish order, "und" in
// Unicode's root order.
func (b *Builder) Order(field, dir string, options ...Opt) *Builder {
	return b.step(func(d *Builder) error {
		k, err := sortKeyOf(field, dir, gather(options).collate, true)
		d.order = append(d.order, k)
		return err
	})
}

// Limit is how many rows come back.
func (b *Builder) Limit(n int) *Builder {
	return b.step(func(d *Builder) (err error) {
		d.limit, err = whole(n, "limit")
		d.hasLimit = true
		return err
	})
}

// Offset is how many rows are passed over first.
func (b *Builder) Offset(n int) *Builder {
	return b.step(func(d *Builder) (err error) {
		d.offset, err = whole(n, "offset")
		return err
	})
}

// ToFenecQL is the statement and its parameters, as they would be sent.
func (b *Builder) ToFenecQL() (string, []any, error) {
	if b.err != nil {
		return "", nil, b.err
	}
	// The engine refuses each of these too; failing here sends nothing.
	if b.group != "" && !b.aggregate {
		return "", nil, refuse("group %s needs an aggregate in select: 'count(*)'", b.group)
	}
	if b.aggregate {
		clash := ""
		switch {
		case b.near != nil:
			clash = "near"
		case b.match != nil:
			clash = "match"
		case len(b.lookups) > 0:
			clash = "lookup"
		case b.count:
			clash = "count"
		}
		if clash != "" {
			return "", nil, refuse("aggregates cannot be combined with %s", clash)
		}
		if b.group == "" && (len(b.order) > 0 || b.hasLimit || b.offset > 0) {
			return "", nil, refuse("aggregates answer one row; group makes a row per value")
		}
	}
	if b.rerank != nil && b.match == nil {
		return "", nil, refuse("rerank needs match: it reorders what match found")
	}
	if b.match != nil && b.near != nil && b.fuse == nil {
		return "", nil, refuse("match and near cannot be combined: both order the result; fuse() ranks by both")
	}
	if b.fuse != nil && (b.match == nil || b.near == nil) {
		return "", nil, refuse("fuse combines match and near: the query needs both")
	}
	if b.fuse != nil && b.rerank != nil {
		return "", nil, refuse("fuse and rerank are two ways to use a vector with match: pick one")
	}
	if len(b.lookups) > 0 {
		clash := ""
		switch {
		case b.near != nil:
			clash = "near"
		case b.match != nil:
			clash = "match"
		case b.rerank != nil:
			clash = "rerank"
		}
		if clash != "" {
			return "", nil, refuse("lookup cannot be combined with %s", clash)
		}
		if b.count && !b.lookups[0].required {
			return "", nil, refuse("count cannot be used with lookup unless it is required: there is nothing to attach children to")
		}
		if len(b.lookups) > MaxLookupDepth {
			return "", nil, refuse("lookup chained too deep: at most %d levels", MaxLookupDepth)
		}
		seen := []string{b.collection}
		for _, l := range b.lookups {
			if slices.Contains(seen, l.collection) {
				return "", nil, refuse("%s cannot look itself up: both sides would answer to the same name", l.collection)
			}
			seen = append(seen, l.collection)
		}
	}
	if b.count {
		if extra := b.extraClause(); extra != "" {
			return "", nil, refuse("count cannot be used with `%s`", extra)
		}
	}
	bind := &binder{params: []any{}}
	var sql strings.Builder
	sql.WriteString("get " + b.collection)
	if len(b.project) > 0 {
		sql.WriteString(" select " + strings.Join(b.project, ", "))
	}
	where, err := whereOf(b.cond, bind)
	if err != nil {
		return "", nil, err
	}
	if where != "" {
		sql.WriteString(" where " + where)
	}
	if b.group != "" {
		sql.WriteString(" group " + b.group)
	}
	if n := b.near; n != nil {
		sql.WriteString(" near " + n.field + " " + bind.bind(n.vector))
		if n.n >= 0 {
			fmt.Fprintf(&sql, " ef %d", n.n)
		}
		if n.exact {
			sql.WriteString(" exact")
		}
	}
	if m := b.match; m != nil {
		sql.WriteString(" match " + m[0].(string) + " " + bind.bind(m[1]))
	}
	if r := b.rerank; r != nil {
		sql.WriteString(" rerank " + r.field + " " + bind.bind(r.vector))
		if r.n >= 0 {
			fmt.Fprintf(&sql, " candidates %d", r.n)
		}
	}
	if f := b.fuse; f != nil {
		sql.WriteString(" fuse")
		if f[0] >= 0 {
			fmt.Fprintf(&sql, " k %d", f[0])
		}
		if f[1] >= 0 {
			fmt.Fprintf(&sql, " candidates %d", f[1])
		}
	}
	writeOrder(&sql, b.order)
	if b.hasLimit {
		fmt.Fprintf(&sql, " limit %d", b.limit)
	}
	if b.offset > 0 {
		fmt.Fprintf(&sql, " offset %d", b.offset)
	}
	if b.count {
		sql.WriteString(" count")
	}
	// Terminal, so every clause after it is the child's -- and last, so its
	// parameters come after the parent's.
	for _, l := range b.lookups {
		sql.WriteString(" lookup " + l.collection + " on " + l.on)
		if l.parent != "" {
			sql.WriteString(" = " + l.parent)
		}
		if l.required {
			sql.WriteString(" required")
		}
		if len(l.project) > 0 {
			sql.WriteString(" select " + strings.Join(l.project, ", "))
		}
		where, err := whereOf(l.cond, bind)
		if err != nil {
			return "", nil, err
		}
		if where != "" {
			sql.WriteString(" where " + where)
		}
		writeOrder(&sql, l.order)
		if l.hasLimit {
			fmt.Fprintf(&sql, " limit %d", l.limit)
		}
		if l.offset > 0 {
			fmt.Fprintf(&sql, " offset %d", l.offset)
		}
	}
	return sql.String(), bind.params, nil
}

func writeOrder(sql *strings.Builder, keys []sortKey) {
	for i, k := range keys {
		if i == 0 {
			sql.WriteString(" order ")
		} else {
			sql.WriteString(", ")
		}
		sql.WriteString(k.String())
	}
}

func whereOf(cond []*node, bind *binder) (string, error) {
	root := prune(&node{t: "and", items: cond})
	if root == nil {
		return "", nil
	}
	return render(root, bind, "")
}

func (b *Builder) extraClause() string {
	switch {
	case b.near != nil:
		return "near"
	case b.match != nil:
		return "match"
	case b.rerank != nil:
		return "rerank"
	case len(b.order) > 0:
		return "order"
	case b.hasLimit:
		return "limit"
	case b.offset > 0:
		return "offset"
	case len(b.project) > 0:
		return "select"
	}
	return ""
}

// Near, Order, Limit mean something only to a read; dropped from a write,
// Limit(1).Delete would delete every row.
func (b *Builder) assertPlain(verb string) error {
	if b.err != nil {
		return b.err
	}
	if extra := b.extraClause(); extra != "" {
		return refuse("%s cannot be used with `%s`", verb, extra)
	}
	if len(b.lookups) > 0 {
		return refuse("%s cannot be used with `lookup`", verb)
	}
	if verb == "insert" && len(b.cond) > 0 {
		return refuse("insert cannot be used with `where`")
	}
	return nil
}

// An update or delete of every row is too easy to do by accident and
// cannot be undone: it has to be asked for, with All.
func (b *Builder) requireFilter(verb string, options []Opt, bind *binder) (string, error) {
	where, err := whereOf(b.cond, bind)
	if err != nil || where != "" {
		return " where " + where, err
	}
	if gather(options).all {
		return "", nil
	}
	return "", refuse("an unfiltered %s covers the whole collection; if you mean it, %s({ all: true })", verb, verb)
}

// Doc is a document whose fields keep the order given, which is the
// statement's: D("title", "Dunes", "year", 2021). A map[string]any is a
// document too, its fields in sorted order, and a struct one in the order
// encoding/json writes it.
type Doc []DocField

// DocField is one field of a Doc.
type DocField struct {
	Name  string
	Value any
}

// D is a Doc of names and values in turn.
func D(pairs ...any) Doc {
	d := make(Doc, 0, len(pairs)/2)
	for i := 0; i+1 < len(pairs); i += 2 {
		d = append(d, DocField{fmt.Sprint(pairs[i]), pairs[i+1]})
	}
	if len(pairs)%2 != 0 {
		d = append(d, DocField{fmt.Sprint(pairs[len(pairs)-1]), missing{}})
	}
	return d
}

// missing is the value of a name D was given none for: refused when written.
type missing struct{}

func docOf(doc any) (Doc, error) {
	switch d := doc.(type) {
	case Doc:
		for _, f := range d {
			if _, ok := f.Value.(missing); ok {
				return nil, refuse("D takes names and values in pairs: %s has none", jsQuote(f.Name))
			}
		}
		return d, nil
	case map[string]any:
		keys := make([]string, 0, len(d))
		for k := range d {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		out := make(Doc, len(keys))
		for i, k := range keys {
			out[i] = DocField{k, d[k]}
		}
		return out, nil
	}
	rv := reflect.ValueOf(doc)
	for rv.Kind() == reflect.Pointer && !rv.IsNil() {
		rv = rv.Elem()
	}
	if rv.Kind() != reflect.Struct {
		return nil, refuse("expected a document object")
	}
	return structDoc(doc)
}

// structDoc reads a struct as encoding/json writes it -- its tags, its
// omitempty -- keeping the order, and the numbers as written.
func structDoc(v any) (Doc, error) {
	raw, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	dec := json.NewDecoder(strings.NewReader(string(raw)))
	dec.UseNumber()
	if _, err := dec.Token(); err != nil {
		return nil, err
	}
	var d Doc
	for dec.More() {
		k, err := dec.Token()
		if err != nil {
			return nil, err
		}
		var val any
		if err := dec.Decode(&val); err != nil {
			return nil, err
		}
		d = append(d, DocField{k.(string), val})
	}
	return d, nil
}

func renderDoc(doc any, bind *binder) (string, error) {
	d, err := docOf(doc)
	if err != nil {
		return "", err
	}
	if len(d) == 0 {
		return "", refuse("cannot write an empty document")
	}
	parts := make([]string, len(d))
	for i, f := range d {
		p, err := fieldPath(f.Name)
		if err != nil {
			return "", err
		}
		parts[i] = p + ": " + bind.bind(f.Value)
	}
	return "{" + strings.Join(parts, ", ") + "}", nil
}

// ToInsert is the put of the documents, not sent.
func (b *Builder) ToInsert(docs ...any) (string, []any, error) {
	if err := b.assertPlain("insert"); err != nil {
		return "", nil, err
	}
	if len(docs) == 0 {
		return "", nil, refuse("cannot write an empty document list")
	}
	bind := &binder{params: []any{}}
	parts := make([]string, len(docs))
	for i, d := range docs {
		s, err := renderDoc(d, bind)
		if err != nil {
			return "", nil, err
		}
		parts[i] = s
	}
	body := strings.Join(parts, ", ")
	if len(docs) > 1 {
		body = "[" + body + "]"
	}
	return "put " + b.collection + " " + body, bind.params, nil
}

// ToUpdate is the set of the rows the filter names, not sent; with no
// filter it is refused unless All is given.
func (b *Builder) ToUpdate(patch any, options ...Opt) (string, []any, error) {
	if err := b.assertPlain("update"); err != nil {
		return "", nil, err
	}
	bind := &binder{params: []any{}}
	body, err := renderDoc(patch, bind)
	if err != nil {
		return "", nil, err
	}
	where, err := b.requireFilter("update", options, bind)
	if err != nil {
		return "", nil, err
	}
	return "set " + b.collection + " " + body + where, bind.params, nil
}

// ToDelete is the del of the rows the filter names, not sent; with no
// filter it is refused unless All is given.
func (b *Builder) ToDelete(options ...Opt) (string, []any, error) {
	if err := b.assertPlain("delete"); err != nil {
		return "", nil, err
	}
	bind := &binder{params: []any{}}
	where, err := b.requireFilter("delete", options, bind)
	if err != nil {
		return "", nil, err
	}
	return "del " + b.collection + where, bind.params, nil
}

// ------------------------------------------------------------- endpoints

func (b *Builder) send(ctx context.Context, text string, params []any) ([]byte, head, error) {
	if b.client == nil {
		return nil, head{}, refuse("query is not bound to a connection: use db.From(...) (ToFenecQL() if you only want the text)")
	}
	return b.client.query(ctx, text, params)
}

func (b *Builder) answer(ctx context.Context) ([]byte, error) {
	text, params, err := b.ToFenecQL()
	if err != nil {
		return nil, err
	}
	raw, _, err := b.send(ctx, text, params)
	return raw, err
}

// Rows runs the query and hands back its rows.
func (b *Builder) Rows(ctx context.Context) ([]Row, error) {
	raw, err := b.answer(ctx)
	if err != nil {
		return nil, err
	}
	return rowsOf(raw)
}

// RowsAs runs the query and decodes its rows into T by their json tags, as
// QueryAs does.
func RowsAs[T any](ctx context.Context, b *Builder) ([]T, error) {
	raw, err := b.answer(ctx)
	if err != nil {
		return nil, err
	}
	var out []T
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, fmt.Errorf("fenecdb: the answer is not rows: %w", err)
	}
	return out, nil
}

// First runs the query with Limit(1) and hands back its row, or nil.
func (b *Builder) First(ctx context.Context) (Row, error) {
	rows, err := b.Limit(1).Rows(ctx)
	if err != nil || len(rows) == 0 {
		return nil, err
	}
	return rows[0], nil
}

// Count is how many rows match: `get ... count`, no row decoded.
func (b *Builder) Count(ctx context.Context) (int64, error) {
	c := b.step(func(d *Builder) error { d.count = true; return nil })
	rows, err := c.Rows(ctx)
	if err != nil || len(rows) == 0 {
		return 0, err
	}
	n, _ := rows[0]["count"].(float64)
	return int64(n), nil
}

// Explain is the path the query took, a line a step; the query runs to
// tell.
func (b *Builder) Explain(ctx context.Context) ([]string, error) {
	text, params, err := b.ToFenecQL()
	if err != nil {
		return nil, err
	}
	raw, _, err := b.send(ctx, "explain "+text, params)
	if err != nil {
		return nil, err
	}
	rows, err := rowsOf(raw)
	lines := make([]string, 0, len(rows))
	for _, r := range rows {
		s, _ := r["plan"].(string)
		lines = append(lines, s)
	}
	return lines, err
}

func (b *Builder) exec(ctx context.Context, text string, params []any) (Result, error) {
	raw, h, err := b.send(ctx, text, params)
	if err != nil {
		return Result{}, err
	}
	var r Result
	if err := json.Unmarshal(raw, &r); err != nil {
		return Result{}, fmt.Errorf("fenecdb: a write answered %q", raw)
	}
	r.Seq, r.Replayed = h.seq, h.replayed
	return r, nil
}

// Insert puts the documents -- Docs, maps or structs -- and hands back how
// many were written. None is no request.
func (b *Builder) Insert(ctx context.Context, docs ...any) (Result, error) {
	if len(docs) == 0 {
		return Result{}, b.err
	}
	text, params, err := b.ToInsert(docs...)
	if err != nil {
		return Result{}, err
	}
	return b.exec(ctx, text, params)
}

// Update sets patch's fields on the rows the filter names; with no filter
// it is refused unless All is given.
func (b *Builder) Update(ctx context.Context, patch any, options ...Opt) (Result, error) {
	text, params, err := b.ToUpdate(patch, options...)
	if err != nil {
		return Result{}, err
	}
	return b.exec(ctx, text, params)
}

// Delete deletes the rows the filter names; with no filter it is refused
// unless All is given.
func (b *Builder) Delete(ctx context.Context, options ...Opt) (Result, error) {
	text, params, err := b.ToDelete(options...)
	if err != nil {
		return Result{}, err
	}
	return b.exec(ctx, text, params)
}
