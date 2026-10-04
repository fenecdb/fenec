package fenecdb_test

// The query builder: held to integrations/builder-golden.json -- the text and
// parameters the JavaScript builder makes of every chain there -- and run
// against the primary, where each answer is the answer to the same
// statement written by hand.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"
	"net/http/httptest"
	"os"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	fenecdb "github.com/fenecdb/fenec/integrations/go"
)

// ------------------------------------------------------------ the golden file

// object is a JSON object with its keys in the order written, which is the
// order a chain's object conditions and documents go into the text.
type object struct {
	keys []string
	vals []any
}

func (o object) get(k string) (any, bool) {
	for i, key := range o.keys {
		if key == k {
			return o.vals[i], true
		}
	}
	return nil, false
}

func decodeOrdered(dec *json.Decoder) (any, error) {
	tok, err := dec.Token()
	if err != nil {
		return nil, err
	}
	switch t := tok.(type) {
	case json.Delim:
		switch t {
		case '{':
			var o object
			for dec.More() {
				k, err := dec.Token()
				if err != nil {
					return nil, err
				}
				v, err := decodeOrdered(dec)
				if err != nil {
					return nil, err
				}
				o.keys, o.vals = append(o.keys, k.(string)), append(o.vals, v)
			}
			_, err := dec.Token()
			return o, err
		case '[':
			list := []any{}
			for dec.More() {
				v, err := decodeOrdered(dec)
				if err != nil {
					return nil, err
				}
				list = append(list, v)
			}
			_, err := dec.Token()
			return list, err
		}
	case json.Number:
		if !strings.ContainsAny(t.String(), ".eE") {
			n, err := t.Int64()
			return int(n), err
		}
		return t.Float64()
	}
	return tok, nil
}

type golden struct {
	name   string
	steps  []object
	text   string
	params any // as the file holds them, plain JSON
	err    string
	isErr  bool
}

func loadGolden(t *testing.T) []golden {
	raw, err := os.ReadFile("../builder-golden.json")
	if err != nil {
		t.Fatal(err)
	}
	dec := json.NewDecoder(strings.NewReader(string(raw)))
	dec.UseNumber()
	all, err := decodeOrdered(dec)
	if err != nil {
		t.Fatal(err)
	}
	var plain []map[string]any
	if err := json.Unmarshal(raw, &plain); err != nil {
		t.Fatal(err)
	}
	var out []golden
	for i, c := range all.([]any) {
		o := c.(object)
		g := golden{params: plain[i]["params"]}
		name, _ := o.get("name")
		g.name = name.(string)
		steps, _ := o.get("steps")
		for _, s := range steps.([]any) {
			g.steps = append(g.steps, s.(object))
		}
		if e, ok := o.get("error"); ok {
			g.err, g.isErr = e.(string), true
		} else {
			text, _ := o.get("text")
			g.text = text.(string)
		}
		out = append(out, g)
	}
	return out
}

// value is an argument as a Go caller would hand it over.
func value(x any) any {
	switch v := x.(type) {
	case []any:
		out := make([]any, len(v))
		for i, e := range v {
			out[i] = value(e)
		}
		return out
	case object:
		if d, ok := v.get("$date"); ok {
			t, err := time.Parse(time.RFC3339Nano, d.(string))
			if err != nil {
				panic(err)
			}
			return t
		}
		if f, ok := v.get("$f32"); ok {
			list := f.([]any)
			out := make([]float32, len(list))
			for i, n := range list {
				out[i] = float32(number(n))
			}
			return out
		}
		if n, ok := v.get("$inc"); ok {
			return fenecdb.Inc(value(n))
		}
		if e, ok := v.get("$expr"); ok {
			list := e.([]any)
			params := make([]any, len(list)-1)
			for i, p := range list[1:] {
				params[i] = value(p)
			}
			return fenecdb.Expr(list[0].(string), params...)
		}
		if len(v.keys) == 1 && strings.HasPrefix(v.keys[0], "$") {
			panic("a condition where a value goes: " + v.keys[0])
		}
		m := make(map[string]any, len(v.keys))
		for i, k := range v.keys {
			m[k] = value(v.vals[i])
		}
		return m
	}
	return x
}

func number(x any) float64 {
	switch n := x.(type) {
	case int:
		return float64(n)
	case float64:
		return n
	}
	panic(fmt.Sprintf("not a number: %v", x))
}

func isPlain(o object) bool {
	for _, k := range o.keys {
		if strings.HasPrefix(k, "$") {
			return false
		}
	}
	return true
}

// cond is a condition argument: an object of fields -- Fields, in order --
// or what Or, And, Not and Raw make.
func cond(x any) fenecdb.Cond {
	o := x.(object)
	if v, ok := o.get("$or"); ok {
		return fenecdb.Or(conds(v)...)
	}
	if v, ok := o.get("$and"); ok {
		return fenecdb.And(conds(v)...)
	}
	if v, ok := o.get("$not"); ok {
		return fenecdb.Not(cond(v))
	}
	if v, ok := o.get("$raw"); ok {
		list := v.([]any)
		params := make([]any, len(list)-1)
		for i, p := range list[1:] {
			params[i] = value(p)
		}
		return fenecdb.Raw(list[0].(string), params...)
	}
	pairs := make([]any, 0, 2*len(o.keys))
	for i, k := range o.keys {
		pairs = append(pairs, k, spec(o.vals[i]))
	}
	return fenecdb.Fields(pairs...)
}

func conds(x any) []fenecdb.Cond {
	var out []fenecdb.Cond
	for _, c := range x.([]any) {
		out = append(out, cond(c))
	}
	return out
}

// spec is what a field is held to in an object condition: an operator
// object, or a value.
func spec(x any) any {
	o, ok := x.(object)
	if !ok || !isPlain(o) {
		return value(x)
	}
	pairs := make([]any, 0, 2*len(o.keys))
	for i, k := range o.keys {
		if k == "not" {
			pairs = append(pairs, k, spec(o.vals[i]))
		} else {
			pairs = append(pairs, k, value(o.vals[i]))
		}
	}
	return fenecdb.Ops(pairs...)
}

// insertOpts is an insert's options, among its documents as Insert takes them.
func insertOpts(o object) []any {
	var out []any
	for i, k := range o.keys {
		if k == "ifAbsent" && o.vals[i].(bool) {
			out = append(out, fenecdb.IfAbsent())
		}
		if k == "require" {
			out = append(out, fenecdb.Require(o.vals[i].(int)))
		}
	}
	return out
}

func optsOf(x []any, at int) object {
	if len(x) > at {
		return x[at].(object)
	}
	return object{}
}

func options(o object) []fenecdb.Opt {
	var out []fenecdb.Opt
	for i, k := range o.keys {
		v := o.vals[i]
		switch k {
		case "ef":
			out = append(out, fenecdb.Ef(v.(int)))
		case "exact":
			if v.(bool) {
				out = append(out, fenecdb.Exact())
			}
		case "k":
			out = append(out, fenecdb.K(v.(int)))
		case "candidates":
			out = append(out, fenecdb.Candidates(v.(int)))
		case "collate":
			out = append(out, fenecdb.Collate(v.(string)))
		case "all":
			if v.(bool) {
				out = append(out, fenecdb.All())
			}
		case "require":
			out = append(out, fenecdb.Require(v.(int)))
		case "on":
			out = append(out, fenecdb.On(v.(string)))
		case "parentKey":
			out = append(out, fenecdb.ParentKey(v.(string)))
		case "required":
			if v.(bool) {
				out = append(out, fenecdb.Required())
			}
		case "limit":
			out = append(out, fenecdb.Limit(v.(int)))
		case "offset":
			out = append(out, fenecdb.Offset(v.(int)))
		case "where":
			out = append(out, fenecdb.Where(cond(v)))
		case "top":
			out = append(out, fenecdb.Top(v.(int)))
		case "pre":
			out = append(out, fenecdb.Pre(v))
		case "post":
			out = append(out, fenecdb.Post(v))
		case "ellipsis":
			out = append(out, fenecdb.Ellipsis(v))
		case "select":
			if s, ok := v.(string); ok {
				out = append(out, fenecdb.Select(s))
			} else {
				out = append(out, fenecdb.Select(strs(v)...))
			}
		case "order":
			if s, ok := v.(string); ok {
				out = append(out, fenecdb.Sort(s, "asc"))
				continue
			}
			for _, key := range v.([]any) {
				if s, ok := key.(string); ok {
					out = append(out, fenecdb.Sort(s, "asc"))
					continue
				}
				parts := key.([]any)
				dir := "asc"
				if len(parts) > 1 {
					dir = parts[1].(string)
				}
				out = append(out, fenecdb.Sort(parts[0].(string), dir, options(optsOf(parts, 2))...))
			}
		default:
			panic("no option " + k)
		}
	}
	return out
}

func strs(x any) []string {
	var out []string
	for _, s := range x.([]any) {
		out = append(out, s.(string))
	}
	return out
}

func docs(x any) []any {
	if list, ok := x.([]any); ok {
		out := make([]any, len(list))
		for i, d := range list {
			out[i] = doc(d)
		}
		return out
	}
	return []any{doc(x)}
}

func doc(x any) fenecdb.Doc {
	o := x.(object)
	d := fenecdb.Doc{}
	for i, k := range o.keys {
		d = append(d, fenecdb.DocField{Name: k, Value: value(o.vals[i])})
	}
	return d
}

// recorder is a server that answers as fenec-server would and keeps the
// last body it was sent.
type recorder struct {
	mu   sync.Mutex
	body []byte
}

func (r *recorder) ServeHTTP(w http.ResponseWriter, req *http.Request) {
	raw, _ := io.ReadAll(req.Body)
	r.mu.Lock()
	r.body = raw
	r.mu.Unlock()
	var q struct{ Query string }
	json.Unmarshal(raw, &q)
	switch {
	case strings.HasPrefix(q.Query, "put ") || strings.HasPrefix(q.Query, "set ") || strings.HasPrefix(q.Query, "del "):
		io.WriteString(w, `{"affected":0}`)
	case strings.HasSuffix(q.Query, " count"):
		io.WriteString(w, `[{"count":0}]`)
	default:
		io.WriteString(w, `[]`)
	}
}

// outcome is what a chain made: its text and parameters as JSON, or the
// refusal's message.
type outcome struct {
	text   string
	params any
	err    string
}

func runChain(t *testing.T, db *fenecdb.Client, rec *recorder, steps []object) outcome {
	ctx := context.Background()
	args := func(s object) []any {
		a, _ := s.get("args")
		if a == nil {
			return nil
		}
		return a.([]any)
	}
	first := steps[0]
	q := db.From(args(first)[0].(string))
	sent := func(err error) outcome {
		if err != nil {
			return refused(t, err)
		}
		var body struct {
			Query  string
			Params any
		}
		if err := json.Unmarshal(rec.body, &body); err != nil {
			t.Fatal(err)
		}
		return outcome{text: body.Query, params: body.Params}
	}
	for i, s := range steps[1:] {
		op, _ := s.get("op")
		a := args(s)
		last := i == len(steps)-2
		switch op {
		case "select":
			q = q.Select(strs(a)...)
		case "where", "orWhere":
			var c fenecdb.Cond
			switch len(a) {
			case 3:
				c = fenecdb.Cmp(a[0].(string), a[1].(string), value(a[2]))
			case 2:
				c = fenecdb.Fields(a[0].(string), spec(a[1]))
			default:
				c = cond(a[0])
			}
			if op == "where" {
				q = q.WhereCond(c)
			} else {
				q = q.OrWhereCond(c)
			}
		case "near":
			q = q.Near(a[0].(string), value(a[1]), options(optsOf(a, 2))...)
		case "rerank":
			q = q.Rerank(a[0].(string), value(a[1]), options(optsOf(a, 2))...)
		case "match":
			q = q.Match(a[0].(string), a[1].(string))
		case "fuse":
			q = q.Fuse(options(optsOf(a, 0))...)
		case "lookup":
			q = q.Lookup(a[0].(string), options(optsOf(a, 1))...)
		case "group":
			q = q.Group(a[0].(string))
		case "highlight":
			q = q.Highlight(a[0].(string), options(optsOf(a, 1))...)
		case "snippet":
			q = q.Snippet(a[0].(string), a[1].(int), options(optsOf(a, 2))...)
		case "facet":
			q = q.Facet(a[0].(string), options(optsOf(a, 1))...)
		case "order":
			dir := "asc"
			if len(a) > 1 {
				dir = a[1].(string)
			}
			q = q.Order(a[0].(string), dir, options(optsOf(a, 2))...)
		case "limit":
			q = q.Limit(a[0].(int))
		case "offset":
			q = q.Offset(a[0].(int))
		default:
			if !last {
				t.Fatalf("%s ends a chain", op)
			}
			var text string
			var params []any
			var err error
			switch op {
			case "toFenecQL":
				text, params, err = q.ToFenecQL()
			case "toInsert":
				text, params, err = q.ToInsert(append(docs(a[0]), insertOpts(optsOf(a, 1))...)...)
			case "toUpdate":
				text, params, err = q.ToUpdate(doc(a[0]), options(optsOf(a, 1))...)
			case "toDelete":
				text, params, err = q.ToDelete(options(optsOf(a, 0))...)
			case "rows":
				_, err = q.Rows(ctx)
				return sent(err)
			case "first":
				_, err = q.First(ctx)
				return sent(err)
			case "count":
				_, err = q.Count(ctx)
				return sent(err)
			case "explain":
				_, err = q.Explain(ctx)
				return sent(err)
			case "insert":
				_, err = q.Insert(ctx, append(docs(a[0]), insertOpts(optsOf(a, 1))...)...)
				return sent(err)
			case "update":
				_, err = q.Update(ctx, doc(a[0]), options(optsOf(a, 1))...)
				return sent(err)
			case "delete":
				_, err = q.Delete(ctx, options(optsOf(a, 0))...)
				return sent(err)
			default:
				t.Fatalf("no builder step %s", op)
			}
			if err != nil {
				return refused(t, err)
			}
			// The parameters as the client would write them.
			raw, err := json.Marshal(params)
			if err != nil {
				t.Fatal(err)
			}
			var back any
			json.Unmarshal(raw, &back)
			return outcome{text: text, params: back}
		}
	}
	t.Fatal("a chain ends with a statement")
	return outcome{}
}

func refused(t *testing.T, err error) outcome {
	var e *fenecdb.QueryError
	if !errors.As(err, &e) {
		t.Fatalf("not a refusal of the builder's: %v", err)
	}
	return outcome{err: e.Message}
}

// sameJSON compares two decoded JSON values, numbers by value: the file
// writes 1 where encoding/json may write 1, and 0.5 where it writes 0.5,
// but an int and a float of one value are one number to the server.
func sameJSON(a, b any) bool {
	switch x := a.(type) {
	case float64:
		y, ok := b.(float64)
		return ok && (x == y || math.IsNaN(x) && math.IsNaN(y))
	case []any:
		y, ok := b.([]any)
		if !ok || len(x) != len(y) {
			return false
		}
		for i := range x {
			if !sameJSON(x[i], y[i]) {
				return false
			}
		}
		return true
	case map[string]any:
		y, ok := b.(map[string]any)
		if !ok || len(x) != len(y) {
			return false
		}
		for k, v := range x {
			if w, ok := y[k]; !ok || !sameJSON(v, w) {
				return false
			}
		}
		return true
	}
	return reflect.DeepEqual(a, b)
}

func TestBuilderMakesWhatTheGoldenFileSays(t *testing.T) {
	cases := loadGolden(t)
	if len(cases) < 60 {
		t.Fatalf("%d cases", len(cases))
	}
	rec := &recorder{}
	srv := httptest.NewServer(rec)
	defer srv.Close()
	db := fenecdb.New(srv.URL)
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := runChain(t, db, rec, c.steps)
			if c.isErr {
				if got.err != c.err {
					t.Fatalf("refused with %q (text %q), want %q", got.err, got.text, c.err)
				}
				return
			}
			if got.err != "" {
				t.Fatalf("refused with %q, want %q", got.err, c.text)
			}
			if got.text != c.text {
				t.Fatalf("text\n got %q\nwant %q", got.text, c.text)
			}
			if !sameJSON(got.params, c.params) {
				t.Fatalf("params\n got %v\nwant %v", got.params, c.params)
			}
		})
	}
}

func TestBuilderIsImmutable(t *testing.T) {
	base := fenecdb.From("articles").Where("year", ">=", 2024)
	a := base.Where("tags", "has", "rust")
	b := base.Limit(3)
	c := base.Where("lang", "=", "tr")
	for _, w := range []struct {
		q    *fenecdb.Builder
		text string
	}{
		{base, "get articles where year >= $1"},
		{a, "get articles where year >= $1 and tags has $2"},
		{b, "get articles where year >= $1 limit 3"},
		{c, "get articles where year >= $1 and lang = $2"},
	} {
		text, _, err := w.q.ToFenecQL()
		if err != nil || text != w.text {
			t.Fatalf("%q, %v; want %q", text, err, w.text)
		}
	}
	// The first refusal is the one that comes back, and a step after it
	// changes nothing.
	bad := base.Where("a b", "=", 1).Limit(-1)
	if _, _, err := bad.ToFenecQL(); err == nil || !strings.Contains(err.Error(), `invalid field name: "a b"`) {
		t.Fatalf("%v", err)
	}
	if _, err := fenecdb.From("t").Rows(context.Background()); err == nil || !strings.Contains(err.Error(), "not bound") {
		t.Fatalf("an unbound query ran: %v", err)
	}
}

func TestBuilderTakesMapsAndStructs(t *testing.T) {
	type note struct {
		Title string    `json:"title"`
		Stars int       `json:"stars,omitempty"`
		Embed []float32 `json:"embed"`
	}
	text, params, err := fenecdb.From("notes").ToInsert(
		note{Title: "a", Embed: []float32{0.5}},
		map[string]any{"title": "b", "stars": 3},
		fenecdb.D("title", "c", "stars", 4),
	)
	if err != nil {
		t.Fatal(err)
	}
	if want := "put notes [{title: $1, embed: $2}, {stars: $3, title: $4}, {title: $5, stars: $6}]"; text != want {
		t.Fatalf("%q", text)
	}
	if len(params) != 6 {
		t.Fatalf("%v", params)
	}
	text, _, err = fenecdb.From("notes").WhereCond(fenecdb.Fields("year", map[string]any{"lt": 2030, "gte": 2020})).ToFenecQL()
	if err != nil || text != "get notes where year >= $1 and year < $2" {
		t.Fatalf("%q %v", text, err)
	}
	when := time.Date(2026, 1, 2, 3, 4, 5, 678_000_000, time.FixedZone("x", 3600))
	_, params, _ = fenecdb.From("notes").Where("at", ">=", when).Where("n", "in", []int{1, 2}).ToFenecQL()
	if !reflect.DeepEqual(params, []any{"2026-01-02T02:04:05.678Z", 1, 2}) {
		t.Fatalf("%v", params)
	}
}

// ------------------------------------------------------ against the primary

type shelfRow struct {
	Title string `json:"title"`
}

func TestBuilderAnswersAreTheTextsAnswers(t *testing.T) {
	ctx := context.Background()
	db := root()
	name, notes := fresh("shelf"), fresh("notes")
	must(db.Exec(ctx, "create collection "+name+" (title text, year int @sorted, lang text @hash, tags [text], body text @text, embed vector<3> @hnsw(cosine))")).of(t)
	must(db.Exec(ctx, "create collection "+notes+" (doc_id int @hash, stars int)")).of(t)
	defer db.Exec(ctx, "drop collection if exists "+name)
	defer db.Exec(ctx, "drop collection if exists "+notes)

	shelf := db.From(name)
	w := must(shelf.Insert(ctx,
		fenecdb.D("title", "Night at the oasis", "year", 2024, "lang", "en", "tags", []string{"desert"}, "body", "a night under the stars at the oasis", "embed", []float32{0.1, 0.2, 0.3}),
		fenecdb.D("title", "Dunes", "year", 2021, "lang", "en", "tags", []string{"desert", "sand"}, "body", "dunes move with the wind", "embed", []float32{0.9, 0.1, 0}),
		fenecdb.D("title", "Kum", "year", 2023, "lang", "tr", "tags", []string{"sand"}, "body", "kum ve rüzgar", "embed", []float32{0.2, 0.8, 0.1}),
	)).of(t)
	if w.Affected != 3 || w.Seq == 0 {
		t.Fatalf("%+v", w)
	}
	must(db.From(notes).Insert(ctx, map[string]any{"doc_id": 1, "stars": 5}, map[string]any{"doc_id": 1, "stars": 3}, map[string]any{"doc_id": 3, "stars": 4})).of(t)

	v := []float32{0.1, 0.2, 0.3}
	for _, p := range []struct {
		q      *fenecdb.Builder
		text   string
		params []any
	}{
		{shelf.Select("title").Where("year", ">=", 2022).Order("year", "desc"),
			"get " + name + " select title where year >= $1 order year desc", []any{2022}},
		{shelf.Select("title").WhereCond(fenecdb.Fields("lang", "en", "tags", fenecdb.Ops("has", "sand"))),
			"get " + name + " select title where lang = $1 and tags has $2", []any{"en", "sand"}},
		{shelf.Select("title").WhereCond(fenecdb.Or(fenecdb.Cmp("lang", "=", "tr"), fenecdb.Cmp("year", "<", 2022))).Order("title", "asc"),
			"get " + name + " select title where lang = $1 or year < $2 order title asc", []any{"tr", 2022}},
		{shelf.Select("title").Near("embed", v).Limit(2),
			"get " + name + " select title near embed $1 limit 2", []any{v}},
		{shelf.Select("title").Match("body", "oasis stars"),
			"get " + name + " select title match body $1", []any{"oasis stars"}},
		{shelf.Select("title").Where("id", "in", []int{1, 3}).Lookup(notes, fenecdb.On("doc_id"), fenecdb.Select("stars"), fenecdb.Sort("stars", "desc")),
			"get " + name + " select title where id in [$1, $2] lookup " + notes + " on doc_id select stars order stars desc", []any{1, 3}},
		{shelf.Select("lang", "count(*)").Group("lang").Order("lang", "asc"),
			"get " + name + " select lang, count(*) group lang order lang asc", nil},
	} {
		text, params, err := p.q.ToFenecQL()
		if err != nil || text != p.text {
			t.Fatalf("%q %v; want %q", text, err, p.text)
		}
		got := must(p.q.Rows(ctx)).of(t)
		want := must(db.Query(ctx, p.text, p.params...)).of(t)
		if len(got) == 0 || !reflect.DeepEqual(got, want) {
			t.Fatalf("%s:\n got %v\nwant %v", text, got, want)
		}
		_ = params
	}
	titles := must(fenecdb.RowsAs[shelfRow](ctx, shelf.Select("title").Order("year", "asc"))).of(t)
	if len(titles) != 3 || titles[0].Title != "Dunes" {
		t.Fatalf("%v", titles)
	}
	if n := must(shelf.Where("lang", "=", "en").Count(ctx)).of(t); n != 2 {
		t.Fatalf("count %d", n)
	}
	if r := must(shelf.Order("year", "asc").First(ctx)).of(t); r["title"] != "Dunes" {
		t.Fatalf("first %v", r)
	}
	if r := must(shelf.Where("lang", "=", "xx").First(ctx)).of(t); r != nil {
		t.Fatalf("first of none %v", r)
	}
	if plan := must(shelf.Near("embed", v).Limit(1).Explain(ctx)).of(t); len(plan) == 0 {
		t.Fatal("no plan")
	}

	if u := must(shelf.Where("lang", "=", "tr").Update(ctx, map[string]any{"year": 2025})).of(t); u.Affected != 1 {
		t.Fatalf("%+v", u)
	}
	if _, err := shelf.Delete(ctx); err == nil {
		t.Fatal("an unfiltered delete went through")
	}
	if d := must(shelf.Where("year", "<", 2022).Delete(ctx)).of(t); d.Affected != 1 {
		t.Fatalf("%+v", d)
	}
	if d := must(shelf.Delete(ctx, fenecdb.All())).of(t); d.Affected != 2 {
		t.Fatalf("%+v", d)
	}
	if n := must(shelf.Count(ctx)).of(t); n != 0 {
		t.Fatalf("count %d", n)
	}
}

// A highlight answers a row's marks as UTF-16 offsets, and a facet's counts
// come beside the rows -- from /query, the builder and /batch alike.
func TestHighlightsAndFacets(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("marks")
	must(db.Exec(ctx, "create collection "+name+" (kind text, body text @text)")).of(t)
	defer db.Exec(ctx, "drop collection if exists "+name)
	docs := db.From(name)
	must(docs.Insert(ctx,
		fenecdb.D("kind", "note", "body", "rust is fast"),
		fenecdb.D("kind", "note", "body", "rust and go"),
		fenecdb.D("kind", "memo", "body", "rust"),
		fenecdb.D("kind", nil, "body", "go only"),
		fenecdb.D("kind", nil, "body", "go again"),
		fenecdb.D("kind", "note", "body", "notes again"),
	)).of(t)

	rows := must(docs.Select("kind").Highlight("body").Match("body", "rust").Where("kind", "=", "memo").Rows(ctx)).of(t)
	if len(rows) != 1 || !reflect.DeepEqual(rows[0]["highlight(body)"], []any{[]any{0.0, 4.0}}) {
		t.Fatalf("%v", rows)
	}
	tagged := must(docs.Highlight("body", fenecdb.Tags("<b>", "</b>")).Snippet("body", 2).Match("body", "fast").First(ctx)).of(t)
	if tagged["highlight(body)"] != "rust is <b>fast</b>" {
		t.Fatalf("%v", tagged)
	}
	if s, ok := tagged["snippet(body)"].(map[string]any); !ok || s["text"] == nil || s["marks"] == nil {
		t.Fatalf("%v", tagged)
	}

	q := docs.Select("body").Facet("kind").Order("body", "asc").Limit(1)
	a := must(q.Answer(ctx)).of(t)
	want := fenecdb.Facets{{Field: "kind", Counts: []fenecdb.FacetCount{{"note", 3}, {nil, 2}, {"memo", 1}}}}
	if len(a.Rows) != 1 || !reflect.DeepEqual(a.Facets, want) {
		t.Fatalf("%+v", a)
	}
	if got := a.Facets.Of("kind"); len(got) != 3 || a.Facets.Of("other") != nil {
		t.Fatalf("%v", got)
	}
	// Rows keeps handing back the rows alone, as RowsAs and Query do.
	if r := must(q.Rows(ctx)).of(t); len(r) != 1 || r[0]["body"] != "go again" {
		t.Fatalf("%v", r)
	}
	if r := must(fenecdb.RowsAs[map[string]string](ctx, q)).of(t); len(r) != 1 {
		t.Fatalf("%v", r)
	}
	text, params, _ := q.ToFenecQL()
	if r := must(db.Query(ctx, text, params...)).of(t); len(r) != 1 {
		t.Fatalf("%v", r)
	}
	if b := must(db.QueryAnswer(ctx, text, params...)).of(t); !reflect.DeepEqual(b.Facets, want) {
		t.Fatalf("%+v", b)
	}
	// A query without facets answers none; a count with them still counts.
	if b := must(docs.Limit(1).Answer(ctx)).of(t); b.Facets != nil || len(b.Rows) != 1 {
		t.Fatalf("%+v", b)
	}
	if n := must(docs.Where("kind", "=", "note").Facet("kind").Count(ctx)).of(t); n != 3 {
		t.Fatalf("count %d", n)
	}
	out := must(db.Batch(ctx, fenecdb.Stmt(text, params...), fenecdb.Stmt("get "+name+" limit 1"))).of(t)
	if len(out.Results) != 2 || !reflect.DeepEqual(out.Results[0].Facets, want) || len(out.Results[0].Rows) != 1 ||
		out.Results[1].Facets != nil || len(out.Results[1].Rows) != 1 {
		t.Fatalf("%+v", out)
	}
}
