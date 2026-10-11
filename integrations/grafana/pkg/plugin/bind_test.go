package plugin

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
	"time"
)

var (
	from = time.Date(2026, 10, 4, 0, 0, 0, 0, time.UTC)
	to   = time.Date(2026, 10, 11, 0, 0, 0, 0, time.UTC)
	bi   = Builtins{From: from, To: to, Interval: time.Hour, MaxDataPoints: 1200}
)

func vars(kv ...any) map[string]VarValue {
	out := map[string]VarValue{}
	for i := 0; i < len(kv); i += 2 {
		switch v := kv[i+1].(type) {
		case string:
			out[kv[i].(string)] = VarValue{Values: []string{v}}
		case []string:
			out[kv[i].(string)] = VarValue{Values: v, Multi: true}
		}
	}
	return out
}

func mustBind(t *testing.T, q Query) Bound {
	t.Helper()
	b, err := Bind(q, bi)
	if err != nil {
		t.Fatalf("%q: %v", q.QueryText, err)
	}
	return b
}

func refused(t *testing.T, q Query, why string) {
	t.Helper()
	_, err := Bind(q, bi)
	if err == nil || !strings.Contains(err.Error(), why) {
		t.Fatalf("%q: want an error saying %q, got %v", q.QueryText, why, err)
	}
}

func TestBuiltinsAreParameters(t *testing.T) {
	b := mustBind(t, Query{QueryText: "get events select bucket(at, $__interval_ms) as t, count(*) as n where at >= $__from and at < ${__to} group t order t limit $__max_data_points"})
	want := "get events select bucket(at, $1) as t, count(*) as n where at >= $2 and at < $3 group t order t limit $4"
	if b.Text != want {
		t.Fatalf("text:\n got %s\nwant %s", b.Text, want)
	}
	if !reflect.DeepEqual(b.Params, []any{int64(3_600_000), from.UnixMilli(), to.UnixMilli(), int64(1200)}) {
		t.Fatalf("params: %#v", b.Params)
	}
	// A rule's interval may be unset: a bucket of no width is refused, so
	// it is a millisecond.
	r, err := Bind(Query{QueryText: "get e select bucket(at, $__interval_ms) as t"}, Builtins{From: from, To: to})
	if err != nil || r.Params[0] != int64(1) {
		t.Fatalf("an unset interval bound %v, %v", r.Params, err)
	}
}

func TestTheListComesFirstAndNamesAfterIt(t *testing.T) {
	b := mustBind(t, Query{
		QueryText: "get ticks where sym = $1 and qty >= ${min:int} and at >= $__from and at < $__to and qty < ${min:int} and px < $2",
		Params:    []Param{{Type: "text", Value: "S01"}, {Type: "float", Value: "12.5"}},
		Vars:      vars("min", "10"),
	})
	want := "get ticks where sym = $1 and qty >= $3 and at >= $4 and at < $5 and qty < $3 and px < $2"
	if b.Text != want {
		t.Fatalf("text:\n got %s\nwant %s", b.Text, want)
	}
	if !reflect.DeepEqual(b.Params, []any{"S01", 12.5, int64(10), from.UnixMilli(), to.UnixMilli()}) {
		t.Fatalf("params: %#v", b.Params)
	}
	refused(t, Query{QueryText: "get ticks where sym = $2", Params: []Param{{Value: "S01"}}}, "$2 has no parameter: the list holds 1")
}

// What a link's ?var-sym= can hold, or a viewer can post to /api/ds/query:
// none of it reaches the statement's text. The text is the same whatever
// the variable holds, and the value is one parameter, as given.
func TestAVariableStaysAValueWhateverItHolds(t *testing.T) {
	attempts := []string{
		`S01`,
		`S01" or sym != "`,
		`S01\" or 1 = 1 --`,
		`"); del ticks where id > 0; --`,
		`S01' or '1' = '1`,
		"S01\n-- a comment\nor true",
		`$1`,
		`$__from`,
		`${sym}`,
		`]` + `, "query": "get ticks", "params": [`,
		`🦊 İstanbul`,
		``,
	}
	for _, a := range attempts {
		b := mustBind(t, Query{QueryText: "get ticks where sym = $sym and at >= $__from", Vars: vars("sym", a)})
		if b.Text != "get ticks where sym = $1 and at >= $2" {
			t.Fatalf("%q changed the text: %s", a, b.Text)
		}
		if b.Params[0] != a {
			t.Fatalf("%q was bound as %#v", a, b.Params[0])
		}
		// And the body the server is sent holds it as one JSON string.
		body, _ := json.Marshal(map[string]any{"query": b.Text, "params": b.Params})
		var back struct {
			Query  string `json:"query"`
			Params []any  `json:"params"`
		}
		if err := json.Unmarshal(body, &back); err != nil || back.Query != b.Text || back.Params[0] != a {
			t.Fatalf("%q did not survive the body: %s", a, body)
		}
	}
	// A parameter's value holding a variable is a value too.
	b := mustBind(t, Query{QueryText: "get ticks where sym = $1", Params: []Param{{Value: "$sym"}}, Vars: vars("sym", `x" or 1 = 1`)})
	if b.Text != "get ticks where sym = $1" || b.Params[0] != `x" or 1 = 1` {
		t.Fatalf("a parameter's variable bound %s %#v", b.Text, b.Params)
	}
	// A literal parameter that looks like a variable is the literal.
	b = mustBind(t, Query{QueryText: "get ticks where sym = $1", Params: []Param{{Value: "$sym and more"}}})
	if b.Params[0] != "$sym and more" {
		t.Fatalf("a literal parameter bound %#v", b.Params)
	}
}

func TestNamesInStringsAndCommentsAreText(t *testing.T) {
	q := "get ticks where note = \"$sym ${sym}\" and tag = 'it\\'s $sym' -- $sym\n# $sym too\nand sym = $sym"
	b := mustBind(t, Query{QueryText: q, Vars: vars("sym", "S01")})
	want := "get ticks where note = \"$sym ${sym}\" and tag = 'it\\'s $sym' -- $sym\n# $sym too\nand sym = $1"
	if b.Text != want || len(b.Params) != 1 {
		t.Fatalf("got %s %v", b.Text, b.Params)
	}
	// A string the server would read to the end of the text is refused, so
	// a name inside it is never bound.
	refused(t, Query{QueryText: `get ticks where sym = "S01 and at >= $__from`}, "a string is not closed")
}

func TestAMultiValueVariableIsAListAfterIn(t *testing.T) {
	v := vars("syms", []string{"S01", "S02", `S03" or 1 = 1`})
	b := mustBind(t, Query{QueryText: "get ticks where sym in $syms and at >= $__from", Vars: v})
	if b.Text != "get ticks where sym in [$1, $2, $3] and at >= $4" {
		t.Fatalf("text: %s", b.Text)
	}
	if !reflect.DeepEqual(b.Params[:3], []any{"S01", "S02", `S03" or 1 = 1`}) {
		t.Fatalf("params: %#v", b.Params)
	}
	b = mustBind(t, Query{QueryText: "get ticks where sym in [${syms}] or sym in $syms", Vars: v})
	if b.Text != "get ticks where sym in [$1, $2, $3] or sym in [$1, $2, $3]" || len(b.Params) != 3 {
		t.Fatalf("in [...]: %s %v", b.Text, b.Params)
	}
	// Nothing picked is an empty list, which matches no row.
	b = mustBind(t, Query{QueryText: "get ticks where sym in $syms", Vars: vars("syms", []string{})})
	if b.Text != "get ticks where sym in []" || len(b.Params) != 0 {
		t.Fatalf("none picked: %s %v", b.Text, b.Params)
	}
	// One picked is that one, anywhere.
	b = mustBind(t, Query{QueryText: "get ticks where sym = $syms", Vars: vars("syms", []string{"S01"})})
	if b.Text != "get ticks where sym = $1" || b.Params[0] != "S01" {
		t.Fatalf("one picked: %s %v", b.Text, b.Params)
	}
	// A single value after `in` is a list of one.
	b = mustBind(t, Query{QueryText: "get ticks where sym in $sym", Vars: vars("sym", "S01")})
	if b.Text != "get ticks where sym in [$1]" {
		t.Fatalf("a single value after in: %s", b.Text)
	}
	refused(t, Query{QueryText: "get ticks where sym = $syms", Vars: v}, "holds 3 values: a list goes after `in`")
	refused(t, Query{QueryText: "get ticks where sym = $1", Params: []Param{{Value: "$syms"}}, Vars: v}, "holds 3 values")
	// Typed, each value is.
	b = mustBind(t, Query{QueryText: "get ticks where qty in ${n:int}", Vars: vars("n", []string{"1", "20"})})
	if !reflect.DeepEqual(b.Params, []any{int64(1), int64(20)}) {
		t.Fatalf("typed list: %#v", b.Params)
	}
}

func TestTypes(t *testing.T) {
	cases := []struct {
		as, in string
		want   any
	}{
		{"", "12", "12"},
		{"text", " a ", " a "},
		{"int", " 12 ", int64(12)},
		{"number", "12", int64(12)},
		{"number", "12.5", 12.5},
		{"float", "3", 3.0},
		{"bool", "true", true},
		{"bool", "0", false},
		{"timestamp", "1759536000000", int64(1759536000000)},
		{"timestamp", "2025-10-04T00:00:00Z", int64(1759536000000)},
		{"timestamp", "2025-10-04", int64(1759536000000)},
		{"json", `{"a": [1, 2]}`, json.RawMessage(`{"a": [1, 2]}`)},
	}
	for _, c := range cases {
		got, err := convert(c.in, c.as)
		if err != nil || !reflect.DeepEqual(got, c.want) {
			t.Fatalf("%s %q: %#v, %v", c.as, c.in, got, err)
		}
	}
	for _, c := range [][2]string{{"int", "1.5"}, {"int", "abc"}, {"float", "NaN"}, {"bool", "yes"}, {"timestamp", "tomorrow"}, {"json", "{"}, {"date", "1"}} {
		if _, err := convert(c[1], c[0]); err == nil {
			t.Fatalf("%s %q was taken", c[0], c[1])
		}
	}
	refused(t, Query{QueryText: "get t where n = ${n:int}", Vars: vars("n", "x")}, `$n: "x" is not an int`)
	refused(t, Query{QueryText: "get t where at >= ${__from:text}"}, "takes no type")
}

func TestOnlyReadsAreSent(t *testing.T) {
	for _, q := range []string{
		`put ticks {sym: "HACK"}`,
		`insert ticks {sym: "HACK"}`,
		`set ticks {qty: 0} where sym = "S01"`,
		`del ticks where id > 0`,
		`drop collection ticks`,
		`create collection x (a int)`,
		`alter collection ticks drop field qty`,
		`compact`,
		"-- a read\n  del ticks",
		"# get\ndel ticks",
	} {
		refused(t, Query{QueryText: q}, "the data source runs reads")
	}
	for _, q := range []string{"get ticks", "  GET ticks", "select sym from ticks", "explain get ticks", "-- note\nget ticks"} {
		mustBind(t, Query{QueryText: q})
	}
	refused(t, Query{QueryText: "   "}, "empty")
}

func TestUnknownNamesAreRefused(t *testing.T) {
	refused(t, Query{QueryText: "get ticks where sym = $sym"}, "an alert rule has none")
	refused(t, Query{QueryText: "get ticks where at >= $__now"}, "none of the names the data source binds")
	refused(t, Query{QueryText: "get ticks where at >= ${a b}"}, "names no variable")
	refused(t, Query{QueryText: "get ticks where at >= ${sym"}, "not closed")
	refused(t, Query{QueryText: "get ticks where at >= $"}, "neither a number nor")
	refused(t, Query{QueryText: "get ticks where at >= $0"}, "numbered from $1")
}

func TestAfter(t *testing.T) {
	b := mustBind(t, Query{QueryText: "get ticks", After: "42"})
	if b.After != 42 {
		t.Fatalf("after: %d", b.After)
	}
	b = mustBind(t, Query{QueryText: "get ticks", After: "$seq", Vars: vars("seq", "77")})
	if b.After != 77 {
		t.Fatalf("after a variable: %d", b.After)
	}
	b = mustBind(t, Query{QueryText: "get ticks", After: "$seq", Vars: vars("seq", "")})
	if b.After != 0 {
		t.Fatalf("after an empty variable: %d", b.After)
	}
	refused(t, Query{QueryText: "get ticks", After: "soon"}, "not a change's number")
}

func TestShape(t *testing.T) {
	cases := []struct {
		q    string
		want Shape
	}{
		{"get ticks select bucket(at, $__interval_ms) as t, sym, sum(qty) as v where at >= $__from group t, sym order t",
			Shape{Verb: "get", Collection: "ticks", GroupFirst: "t"}},
		{"select title, year from articles where year > 2000", Shape{Verb: "select", Collection: "articles"}},
		{"explain get ticks group bucket(at, 1h)", Shape{Verb: "explain", Collection: "ticks", GroupFirst: "bucket"}},
		{"get authors select name lookup books on author = id select title lookup reviews on book = id group stars",
			Shape{Verb: "get", Collection: "authors", Lookups: []string{"books", "reviews"}}},
	}
	for _, c := range cases {
		b := mustBind(t, Query{QueryText: c.q})
		if !reflect.DeepEqual(b.Shape, c.want) {
			t.Fatalf("%q: %+v, want %+v", c.q, b.Shape, c.want)
		}
	}
}

func TestVarValuesFromTheBrowser(t *testing.T) {
	var q Query
	if err := json.Unmarshal([]byte(`{"queryText": "get t", "vars": {"a": "x", "b": ["1", 2, true], "c": 3.5}}`), &q); err != nil {
		t.Fatal(err)
	}
	want := map[string]VarValue{
		"a": {Values: []string{"x"}},
		"b": {Values: []string{"1", "2", "true"}, Multi: true},
		"c": {Values: []string{"3.5"}},
	}
	if !reflect.DeepEqual(q.Vars, want) {
		t.Fatalf("%+v", q.Vars)
	}
	if err := json.Unmarshal([]byte(`{"vars": {"a": {"x": 1}}}`), &q); err == nil {
		t.Fatal("an object was taken for a variable's value")
	}
}
