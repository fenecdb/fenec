package plugin

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/grafana/grafana-plugin-sdk-go/data"
)

func framesOf(t *testing.T, raw string, schemas Schemas, o FrameOptions) data.Frames {
	t.Helper()
	a, err := parseAnswer([]byte(raw))
	if err != nil {
		t.Fatal(err)
	}
	if o.RefID == "" {
		o.RefID = "A"
	}
	if o.Format == "" {
		o.Format = "auto"
	}
	f, err := Frames(a, schemas, o)
	if err != nil {
		t.Fatal(err)
	}
	return f
}

func names(f *data.Frame) []string {
	out := make([]string, len(f.Fields))
	for i, x := range f.Fields {
		out[i] = x.Name
	}
	return out
}

func fieldTypes(f *data.Frame) []data.FieldType {
	out := make([]data.FieldType, len(f.Fields))
	for i, x := range f.Fields {
		out[i] = x.Type()
	}
	return out
}

func at(f *data.Field, i int) any {
	v, ok := f.ConcreteAt(i)
	if !ok {
		return nil
	}
	return v
}

func ts(s string) time.Time {
	t, err := time.Parse(time.RFC3339Nano, s)
	if err != nil {
		panic(err)
	}
	return t
}

var tickSchema = Schemas{"ticks": {
	"sym": "text", "at": "timestamp", "px": "float", "qty": "int", "loc": "geo",
	"meta": "json", "v": "vector<3>", "tags": "[text]", "ok": "bool", "raw": "bytes",
	"sp": "sparse<10>", "note": "text",
}}

// The columns come in the order the select list gave them, typed by the
// schema where they read a field and by their values where computed.
func TestColumnsInTheSelectListsOrderAndTyped(t *testing.T) {
	raw := `[{"qty":3,"sym":"S01","at":"2026-10-04T12:00:00Z","px":12,"ok":true,"note":"2026-10-04T12:00:00Z","n":2,"avg":1.5,"t":"2026-10-04T00:00:00.250Z","meta":{"a":1},"tags":["x","y"],"raw":[1,2]},` +
		`{"qty":null,"sym":"S02","at":null,"px":12.5,"ok":false,"note":null,"n":4,"avg":2,"t":null,"meta":null,"tags":null,"raw":null}]`
	f := framesOf(t, raw, tickSchema, FrameOptions{Shape: Shape{Collection: "ticks"}})[0]
	if got, want := names(f), []string{"qty", "sym", "at", "px", "ok", "note", "n", "avg", "t", "meta", "tags", "raw"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("names %v, want %v", got, want)
	}
	want := []data.FieldType{
		data.FieldTypeNullableInt64, data.FieldTypeNullableString, data.FieldTypeNullableTime,
		data.FieldTypeNullableFloat64, data.FieldTypeNullableBool,
		// A text field holding what looks like a time stays text.
		data.FieldTypeNullableString,
		data.FieldTypeNullableInt64, data.FieldTypeNullableFloat64,
		// Computed: the server's form of a time is a time.
		data.FieldTypeNullableTime,
		data.FieldTypeNullableJSON, data.FieldTypeNullableJSON, data.FieldTypeNullableJSON,
	}
	if got := fieldTypes(f); !reflect.DeepEqual(got, want) {
		t.Fatalf("types %v\nwant %v", got, want)
	}
	if at(f.Fields[2], 0) != ts("2026-10-04T12:00:00Z") || at(f.Fields[8], 0) != ts("2026-10-04T00:00:00.250Z") {
		t.Fatalf("times: %v %v", at(f.Fields[2], 0), at(f.Fields[8], 0))
	}
	// A float the server wrote whole is still the float column's.
	if at(f.Fields[3], 0) != 12.0 || at(f.Fields[3], 1) != 12.5 || at(f.Fields[0], 1) != nil {
		t.Fatalf("numbers: %v %v %v", at(f.Fields[3], 0), at(f.Fields[3], 1), at(f.Fields[0], 1))
	}
	if string(at(f.Fields[10], 0).(json.RawMessage)) != `["x","y"]` {
		t.Fatalf("a list: %s", at(f.Fields[10], 0))
	}
	if f.Meta.Type != data.FrameTypeTable {
		t.Fatalf("a table's type: %v", f.Meta.Type)
	}
}

func TestNumbersKeepWhatTheServerWrote(t *testing.T) {
	f := framesOf(t, `[{"id":9007199254740993,"n":1,"x":1},{"id":2,"n":2.5,"x":null}]`, nil, FrameOptions{})[0]
	if f.Fields[0].Type() != data.FieldTypeNullableInt64 || at(f.Fields[0], 0) != int64(9007199254740993) {
		t.Fatalf("an id past 2^53: %v %v", f.Fields[0].Type(), at(f.Fields[0], 0))
	}
	if f.Fields[1].Type() != data.FieldTypeNullableFloat64 || at(f.Fields[1], 0) != 1.0 {
		t.Fatalf("ints and floats: %v", f.Fields[1].Type())
	}
	// All null and computed: a number column with nothing in it.
	g := framesOf(t, `[{"s":null}]`, nil, FrameOptions{})[0]
	if g.Fields[0].Type() != data.FieldTypeNullableFloat64 {
		t.Fatalf("all null: %v", g.Fields[0].Type())
	}

}

func TestPointsVectorsAndSparseVectors(t *testing.T) {
	raw := `[{"sym":"S01","loc":[28.97,41.01],"v":[0.1,0.2,0.3],"sp":"{1:0.5}/10"},{"sym":"S02","loc":null,"v":null,"sp":null}]`
	f := framesOf(t, raw, tickSchema, FrameOptions{Shape: Shape{Collection: "ticks"}})[0]
	if got := names(f); !reflect.DeepEqual(got, []string{"sym", "loc.lon", "loc.lat"}) {
		t.Fatalf("names %v", got)
	}
	if at(f.Fields[1], 0) != 28.97 || at(f.Fields[2], 0) != 41.01 || at(f.Fields[1], 1) != nil {
		t.Fatalf("a point: %v %v", at(f.Fields[1], 0), at(f.Fields[2], 0))
	}
	var notes []string
	for _, n := range f.Meta.Notices {
		notes = append(notes, n.Text)
	}
	if len(notes) != 2 || !strings.Contains(notes[0], "v, a vector<3>, is left out") || !strings.Contains(notes[1], "sp, a sparse<10>") {
		t.Fatalf("notices: %v", notes)
	}
	// A vector under another name, which no schema types, is its JSON.
	g := framesOf(t, `[{"e":[0.1,0.2]}]`, tickSchema, FrameOptions{Shape: Shape{Collection: "ticks"}})[0]
	if g.Fields[0].Type() != data.FieldTypeNullableJSON {
		t.Fatalf("an unknown list: %v", g.Fields[0].Type())
	}
}

// A group by time and a label is a series a label: long to wide, as an
// alert rule evaluates it, one instance a symbol.
func TestLongToWide(t *testing.T) {
	raw := `[{"t":"2026-10-04T01:00:00Z","sym":"S02","volume":5,"note":{"x":1}},` +
		`{"t":"2026-10-04T00:00:00Z","sym":"S01","volume":1,"note":null},` +
		`{"t":"2026-10-04T00:00:00Z","sym":"S02","volume":2,"note":null},` +
		`{"t":"2026-10-04T01:00:00Z","sym":"S01","volume":4,"note":null},` +
		`{"t":null,"sym":"S01","volume":9,"note":null}]`
	shape := Shape{Collection: "ticks", GroupFirst: "t"}
	f := framesOf(t, raw, tickSchema, FrameOptions{Shape: shape})[0]
	if f.Meta.Type != data.FrameTypeTimeSeriesWide {
		t.Fatalf("type %v", f.Meta.Type)
	}
	if got := names(f); !reflect.DeepEqual(got, []string{"t", "volume", "volume"}) {
		t.Fatalf("names %v", got)
	}
	if f.Fields[1].Labels["sym"] != "S01" || f.Fields[2].Labels["sym"] != "S02" {
		t.Fatalf("labels %v %v", f.Fields[1].Labels, f.Fields[2].Labels)
	}
	if at(f.Fields[0], 0) != ts("2026-10-04T00:00:00Z") || at(f.Fields[1], 0) != int64(1) || at(f.Fields[2], 1) != int64(5) {
		t.Fatalf("values %v %v %v", at(f.Fields[0], 0), at(f.Fields[1], 0), at(f.Fields[2], 1))
	}
	if f.TimeSeriesSchema().Type != data.TimeSeriesTypeWide {
		t.Fatalf("not a wide series: %v", f.TimeSeriesSchema().Type)
	}
	var notes []string
	for _, n := range f.Meta.Notices {
		notes = append(notes, n.Text)
	}
	if strings.Join(notes, "|") != "a time series holds a time, numbers and text labels: note left out|1 rows with no time left out of the series" {
		t.Fatalf("notices %v", notes)
	}
	// As a table, the same answer is the rows as they came.
	tbl := framesOf(t, raw, tickSchema, FrameOptions{Shape: shape, Format: "table"})[0]
	if tbl.Meta.Type != data.FrameTypeTable || len(tbl.Fields) != 4 {
		t.Fatalf("table: %v %v", tbl.Meta.Type, names(tbl))
	}
	// Auto takes a time first and grouped first for a series, and nothing
	// else.
	if g := framesOf(t, raw, tickSchema, FrameOptions{Shape: Shape{Collection: "ticks"}})[0]; g.Meta.Type != data.FrameTypeTable {
		t.Fatalf("auto without a group: %v", g.Meta.Type)
	}
	if g := framesOf(t, raw, tickSchema, FrameOptions{Shape: Shape{Collection: "ticks"}, Format: "time_series"})[0]; g.Meta.Type != data.FrameTypeTimeSeriesWide {
		t.Fatalf("time_series asked: %v", g.Meta.Type)
	}
	a, _ := parseAnswer([]byte(`[{"sym":"S01","n":1}]`))
	if _, err := Frames(a, nil, FrameOptions{Format: "time_series"}); err == nil || !strings.Contains(err.Error(), "needs a timestamp as the first column") {
		t.Fatalf("a series with no time first: %v", err)
	}
	// No rows: an empty series, no error -- an alert rule's no data.
	e := framesOf(t, `[]`, nil, FrameOptions{Format: "time_series"})[0]
	if len(e.Fields) != 0 || e.Meta.Type != data.FrameTypeTimeSeriesWide {
		t.Fatalf("an empty series: %v %v", names(e), e.Meta.Type)
	}
}

func TestAWideSeriesStaysAsItCame(t *testing.T) {
	raw := `[{"t":"2026-10-04T00:00:00Z","events":3,"visitors":2},{"t":"2026-10-04T01:00:00Z","events":5,"visitors":4}]`
	f := framesOf(t, raw, nil, FrameOptions{Shape: Shape{GroupFirst: "t"}})[0]
	if f.Meta.Type != data.FrameTypeTimeSeriesWide || !reflect.DeepEqual(names(f), []string{"t", "events", "visitors"}) {
		t.Fatalf("%v %v", f.Meta.Type, names(f))
	}
	if f.Fields[0].Type() != data.FieldTypeTime {
		t.Fatalf("a series' time is not nullable: %v", f.Fields[0].Type())
	}
}

func TestFacetsAreFramesOfTheirOwn(t *testing.T) {
	raw := `{"rows":[{"sym":"S01"}],"facets":{"sym":[{"value":"S01","count":3},{"value":null,"count":1}],"qty":[{"value":[1,10],"count":2}]}}`
	fs := framesOf(t, raw, tickSchema, FrameOptions{Shape: Shape{Collection: "ticks"}})
	if len(fs) != 3 || fs[1].Name != "facet sym" || fs[2].Name != "facet qty" {
		t.Fatalf("frames: %d", len(fs))
	}
	if !reflect.DeepEqual(names(fs[1]), []string{"sym", "count"}) || at(fs[1].Fields[0], 0) != "S01" || at(fs[1].Fields[1], 0) != int64(3) {
		t.Fatalf("a facet: %v", names(fs[1]))
	}
	if fs[2].Fields[0].Type() != data.FieldTypeNullableJSON {
		t.Fatalf("a range's value: %v", fs[2].Fields[0].Type())
	}
}

// A lookup's rows nest in each parent's; a frame is flat, as every flat
// transport has them: one row per path, nulls where a level ran out.
func TestLookupsAreFlattened(t *testing.T) {
	raw := `[{"name":"Ann","books":[{"title":"A","reviews":[{"stars":5},{"stars":3}]},{"title":"B","reviews":[]}]},` +
		`{"name":"Bob","books":[]}]`
	f := framesOf(t, raw, Schemas{"reviews": {"stars": "int"}}, FrameOptions{Shape: Shape{Collection: "authors", Lookups: []string{"books", "reviews"}}})[0]
	if got := names(f); !reflect.DeepEqual(got, []string{"name", "books.title", "reviews.stars"}) {
		t.Fatalf("names %v", got)
	}
	var rows [][]any
	for i := 0; i < f.Rows(); i++ {
		rows = append(rows, []any{at(f.Fields[0], i), at(f.Fields[1], i), at(f.Fields[2], i)})
	}
	want := [][]any{{"Ann", "A", int64(5)}, {"Ann", "A", int64(3)}, {"Ann", "B", nil}, {"Bob", nil, nil}}
	if !reflect.DeepEqual(rows, want) {
		t.Fatalf("rows %v", rows)
	}
}

func TestAWritesAnswerIsNoRows(t *testing.T) {
	for _, raw := range []string{`{"affected":1}`, `{"message":"created"}`, `{"rows":[],"x":1}`} {
		if _, err := parseAnswer([]byte(raw)); err == nil || !strings.Contains(err.Error(), "answered no rows") {
			t.Fatalf("%s: %v", raw, err)
		}
	}
}
