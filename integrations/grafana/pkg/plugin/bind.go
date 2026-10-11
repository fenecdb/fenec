package plugin

import (
	"encoding/json"
	"fmt"
	"math"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Query is a panel's query as the editor writes it and the frontend sends
// it: FenecQL with `$n` parameters and the parameters' list, and the
// dashboard variables it names, resolved by the browser (Vars). An alert
// rule sends the same JSON from Grafana's backend, with no Vars.
type Query struct {
	QueryText string  `json:"queryText"`
	Params    []Param `json:"params"`
	// Format is "auto" (a time series when the statement groups by its
	// first column, a time), "table" or "time_series".
	Format string `json:"format"`
	// After is a change the server must hold before it answers
	// (Fenec-After): a number, or one variable that holds one.
	After string `json:"after"`
	// Vars are the dashboard variables the text and the parameters name,
	// each one value or a list of them, as the browser resolved them.
	Vars map[string]VarValue `json:"vars"`
}

// Param is one of `$1`, `$2`, ...: a literal of its type, or one variable
// whole, `$sym` or `${sym}`. A value is never spliced into the text.
type Param struct {
	Type  string `json:"type"`
	Value string `json:"value"`
}

// VarValue is a variable's value: one, or the list a multi-value variable
// holds (Multi), in the order Grafana gives them.
type VarValue struct {
	Values []string
	Multi  bool
}

// MarshalJSON writes the value as the browser sends it.
func (v VarValue) MarshalJSON() ([]byte, error) {
	if v.Multi || len(v.Values) != 1 {
		if v.Values == nil {
			return []byte("[]"), nil
		}
		return json.Marshal(v.Values)
	}
	return json.Marshal(v.Values[0])
}

// UnmarshalJSON takes what the browser sends: a text, a number, a bool, or
// a list of them.
func (v *VarValue) UnmarshalJSON(raw []byte) error {
	var x any
	if err := json.Unmarshal(raw, &x); err != nil {
		return err
	}
	one := func(x any) (string, error) {
		switch x := x.(type) {
		case string:
			return x, nil
		case float64:
			return strconv.FormatFloat(x, 'f', -1, 64), nil
		case bool:
			return strconv.FormatBool(x), nil
		}
		return "", fmt.Errorf("a variable's value is a text, a number or a list of them, not %s", raw)
	}
	if list, ok := x.([]any); ok {
		v.Multi = true
		v.Values = make([]string, 0, len(list))
		for _, x := range list {
			s, err := one(x)
			if err != nil {
				return err
			}
			v.Values = append(v.Values, s)
		}
		return nil
	}
	s, err := one(x)
	if err != nil {
		return err
	}
	*v = VarValue{Values: []string{s}}
	return nil
}

// Builtins are what Grafana gives every query, a dashboard's and an alert
// rule's alike: worked out in the backend from the request, so a rule binds
// them as a panel does.
type Builtins struct {
	From, To      time.Time
	Interval      time.Duration
	MaxDataPoints int64
}

// The names the backend binds itself. Grafana's own: a dashboard writes
// them the same way into every data source's queries.
const (
	nameFrom          = "__from"
	nameTo            = "__to"
	nameIntervalMs    = "__interval_ms"
	nameMaxDataPoints = "__max_data_points"
)

// Bound is a statement ready to send: its text, every name made a `$n`,
// and its parameters in their numbers' order.
type Bound struct {
	Text   string
	Params []any
	Shape  Shape
	After  uint64
}

// Bind makes a query's text and parameters into the statement the server
// is sent. Values go only into the parameters: the text changes only where
// a name of a variable stood, which becomes `$n` -- or, after `in`, a
// bracket of them, one a value, since FenecQL's `in` takes a list of
// values and a parameter binds one. So a variable can hold anything at all
// and stay a value: a quote, a comment, a statement.
func Bind(q Query, b Builtins) (Bound, error) {
	text := q.QueryText
	if strings.TrimSpace(text) == "" {
		return Bound{}, fmt.Errorf("the query is empty")
	}
	toks, err := scan(text)
	if err != nil {
		return Bound{}, err
	}
	shape := shapeOf(toks)
	switch shape.Verb {
	case "get", "select", "explain":
	default:
		// A data source reads. The token's policy is what holds it to
		// that on the server; refused here too, a write never leaves
		// Grafana.
		return Bound{}, fmt.Errorf("the data source runs reads: a statement starting with `get`, `select` or `explain`, not `%s`", firstWord(toks))
	}
	bd := binder{q: q, b: b, slots: map[string][]int{}}
	for _, p := range q.Params {
		v, err := bd.param(p)
		if err != nil {
			return Bound{}, fmt.Errorf("$%d: %w", len(bd.params)+1, err)
		}
		bd.params = append(bd.params, v)
	}
	listed := len(bd.params)

	var out strings.Builder
	last := 0
	for i, t := range toks {
		switch t.kind {
		case tokParam:
			if t.n > listed {
				return Bound{}, fmt.Errorf("position %d: $%d has no parameter: the list holds %d", t.start, t.n, listed)
			}
		case tokRef:
			afterIn := i > 0 && toks[i-1].kind == tokWord && strings.EqualFold(toks[i-1].text, "in")
			// `in [$syms]`: the variable alone inside an `in`'s list.
			inList := i > 1 && i+1 < len(toks) &&
				toks[i-1].text == "[" && toks[i+1].text == "]" &&
				toks[i-2].kind == tokWord && strings.EqualFold(toks[i-2].text, "in")
			slots, err := bd.ref(t, afterIn || inList)
			if err != nil {
				return Bound{}, err
			}
			out.WriteString(text[last:t.start])
			switch {
			case afterIn:
				out.WriteString("[" + numbered(slots) + "]")
			default:
				out.WriteString(numbered(slots))
			}
			last = t.end
		}
	}
	out.WriteString(text[last:])

	bound := Bound{Text: out.String(), Params: bd.params, Shape: shape}
	if a := strings.TrimSpace(q.After); a != "" {
		n, err := bd.after(a)
		if err != nil {
			return Bound{}, err
		}
		bound.After = n
	}
	return bound, nil
}

func firstWord(toks []token) string {
	for _, t := range toks {
		if t.kind == tokWord {
			return t.text
		}
	}
	if len(toks) > 0 {
		return toks[0].text
	}
	return ""
}

// numbered is `$5`, or `$5, $6, $7` for a list.
func numbered(slots []int) string {
	parts := make([]string, len(slots))
	for i, s := range slots {
		parts[i] = "$" + strconv.Itoa(s)
	}
	return strings.Join(parts, ", ")
}

type binder struct {
	q      Query
	b      Builtins
	params []any
	// slots are the parameters a name was given, by name and type: a name
	// used twice is bound once.
	slots map[string][]int
}

// value is what a name holds: a builtin's number, or a variable's values.
func (bd *binder) value(name, as string) ([]any, bool, error) {
	switch name {
	case nameFrom, nameTo, nameIntervalMs, nameMaxDataPoints:
		if as != "" {
			return nil, false, fmt.Errorf("$%s is Grafana's, in milliseconds: it takes no type", name)
		}
		switch name {
		case nameFrom:
			return []any{bd.b.From.UnixMilli()}, false, nil
		case nameTo:
			return []any{bd.b.To.UnixMilli()}, false, nil
		case nameIntervalMs:
			// Grafana's interval is never under a millisecond; a rule's
			// may be unset.
			return []any{max(bd.b.Interval.Milliseconds(), 1)}, false, nil
		default:
			return []any{bd.b.MaxDataPoints}, false, nil
		}
	}
	v, ok := bd.q.Vars[name]
	if !ok {
		if strings.HasPrefix(name, "__") {
			return nil, false, fmt.Errorf("$%s is none of the names the data source binds: $__from, $__to, $__interval_ms, $__max_data_points", name)
		}
		return nil, false, fmt.Errorf("$%s has no value: a dashboard variable of that name binds it, and an alert rule has none", name)
	}
	out := make([]any, 0, len(v.Values))
	for _, s := range v.Values {
		x, err := convert(s, as)
		if err != nil {
			return nil, false, fmt.Errorf("$%s: %w", name, err)
		}
		out = append(out, x)
	}
	return out, v.Multi, nil
}

// ref gives a name in the text its parameters: one, or after `in` one a
// value of its list.
func (bd *binder) ref(t token, list bool) ([]int, error) {
	key := t.name + ":" + t.as
	if s, ok := bd.slots[key]; ok {
		if len(s) != 1 && !list {
			return nil, fmt.Errorf("position %d: $%s holds %d values: a list goes after `in`", t.start, t.name, len(s))
		}
		return s, nil
	}
	values, _, err := bd.value(t.name, t.as)
	if err != nil {
		return nil, fmt.Errorf("position %d: %w", t.start, err)
	}
	// A multi-value variable with one value picked is that value; with
	// more, or none, it is a list, which only `in` takes.
	if !list && len(values) != 1 {
		return nil, fmt.Errorf("position %d: $%s holds %d values: a list goes after `in`, as in `where sym in $%s`", t.start, t.name, len(values), t.name)
	}
	slots := make([]int, len(values))
	for i, v := range values {
		bd.params = append(bd.params, v)
		slots[i] = len(bd.params)
	}
	bd.slots[key] = slots
	return slots, nil
}

// param is one of the list's parameters: one variable whole, or a literal.
func (bd *binder) param(p Param) (any, error) {
	v := strings.TrimSpace(p.Value)
	if strings.HasPrefix(v, "$") {
		t, err := scanDollar(v, 0)
		if err == nil && t.kind == tokRef && t.end == len(v) {
			as := t.as
			if as == "" {
				as = p.Type
			}
			values, multi, err := bd.value(t.name, as)
			if err != nil {
				return nil, err
			}
			if as == "json" && multi {
				return json.RawMessage(mustJSON(values)), nil
			}
			if len(values) != 1 {
				return nil, fmt.Errorf("$%s holds %d values: a list goes in the text, after `in`", t.name, len(values))
			}
			return values[0], nil
		}
	}
	return convert(p.Value, p.Type)
}

func mustJSON(v any) []byte {
	out, _ := json.Marshal(v)
	return out
}

// after is the change a query waits for: digits, or one variable holding
// them.
func (bd *binder) after(a string) (uint64, error) {
	if strings.HasPrefix(a, "$") {
		t, err := scanDollar(a, 0)
		if err != nil || t.kind != tokRef || t.end != len(a) {
			return 0, fmt.Errorf("after: %q is neither a change's number nor one variable", a)
		}
		values, _, err := bd.value(t.name, "")
		if err != nil {
			return 0, fmt.Errorf("after: %w", err)
		}
		if len(values) != 1 {
			return 0, fmt.Errorf("after: $%s holds %d values", t.name, len(values))
		}
		a, _ = values[0].(string)
		if a == "" {
			// The variable is empty: nothing to wait for.
			return 0, nil
		}
	}
	n, err := strconv.ParseUint(strings.TrimSpace(a), 10, 64)
	if err != nil {
		return 0, fmt.Errorf("after: %q is not a change's number", a)
	}
	return n, nil
}

// The types a value is read as: a parameter's, or a name's after `:`.
var types = []string{"text", "number", "int", "float", "bool", "timestamp", "json"}

// convert reads a value given as text into the JSON value the server is
// sent for it. A value that is not of its type is refused, never sent as
// a text in its place.
func convert(s, as string) (any, error) {
	switch as {
	case "", "text":
		return s, nil
	case "number":
		t := strings.TrimSpace(s)
		if n, err := strconv.ParseInt(t, 10, 64); err == nil {
			return n, nil
		}
		f, err := strconv.ParseFloat(t, 64)
		if err != nil || math.IsNaN(f) || math.IsInf(f, 0) {
			return nil, fmt.Errorf("%q is not a number", s)
		}
		return f, nil
	case "int":
		n, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64)
		if err != nil {
			return nil, fmt.Errorf("%q is not an int", s)
		}
		return n, nil
	case "float":
		f, err := strconv.ParseFloat(strings.TrimSpace(s), 64)
		if err != nil || math.IsNaN(f) || math.IsInf(f, 0) {
			return nil, fmt.Errorf("%q is not a float", s)
		}
		return f, nil
	case "bool":
		switch strings.ToLower(strings.TrimSpace(s)) {
		case "true", "1":
			return true, nil
		case "false", "0":
			return false, nil
		}
		return nil, fmt.Errorf("%q is not a bool", s)
	case "timestamp":
		// Milliseconds since the epoch, which a timestamp field compares
		// with as it is, or a time as text, made milliseconds here.
		t := strings.TrimSpace(s)
		if n, err := strconv.ParseInt(t, 10, 64); err == nil {
			return n, nil
		}
		for _, layout := range []string{time.RFC3339Nano, "2006-01-02T15:04:05", "2006-01-02 15:04:05", "2006-01-02"} {
			if at, err := time.Parse(layout, t); err == nil {
				return at.UnixMilli(), nil
			}
		}
		return nil, fmt.Errorf("%q is neither milliseconds since the epoch nor a time like 2026-10-04T12:00:00Z", s)
	case "json":
		if !json.Valid([]byte(s)) {
			return nil, fmt.Errorf("%q is not JSON", s)
		}
		return json.RawMessage(s), nil
	}
	sorted := append([]string(nil), types...)
	sort.Strings(sorted)
	return nil, fmt.Errorf("no type %q: one of %s", as, strings.Join(sorted, ", "))
}
