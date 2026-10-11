package plugin

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/grafana/grafana-plugin-sdk-go/data"
)

// The answer carries no types -- a float written whole is `12`, a timestamp
// is ISO text -- so a column's type comes from the collection's schema
// where the column is one of its fields, and from its values where it is
// computed (`bucket(at, 1h) as t`, `count(*)`).

// Schemas are the collections' field types, by collection and field, as
// the `collections` statement lists them: "int", "timestamp", "geo",
// "vector<3>", "[text]", ...
type Schemas map[string]map[string]string

// FrameOptions say how a table becomes frames.
type FrameOptions struct {
	RefID  string
	Format string // auto, table, time_series
	Shape  Shape
	// Executed is the statement as sent, for the query inspector.
	Executed string
}

// Frames builds a read's frames: its rows as one table, or a time series,
// and then a frame per facet.
func Frames(a answer, schemas Schemas, o FrameOptions) (data.Frames, error) {
	t := flatten(a.rows, o.Shape.Lookups)
	// No name: a frame's name goes before each field's in a legend.
	frame := data.NewFrame("")
	frame.RefID = o.RefID
	var notices []data.Notice
	for i, name := range t.names {
		fields, note := fieldsOf(name, t.cols[i], hintOf(name, schemas, o.Shape))
		frame.Fields = append(frame.Fields, fields...)
		if note != "" {
			notices = append(notices, data.Notice{Severity: data.NoticeSeverityInfo, Text: note})
		}
	}
	frame.Meta = &data.FrameMeta{
		Type:                data.FrameTypeTable,
		TypeVersion:         data.FrameTypeVersion{0, 1},
		ExecutedQueryString: o.Executed,
		Notices:             notices,
	}
	series, err := timeSeries(frame, o)
	if err != nil {
		return nil, err
	}
	frames := data.Frames{series}
	for _, f := range a.facets {
		ff := data.NewFrame("facet "+f.field, nil...)
		ff.RefID = o.RefID
		values, _ := fieldsOf(f.field, f.values, hintOf(f.field, schemas, o.Shape))
		counts := data.NewField("count", nil, f.counts)
		if len(values) == 0 {
			values = []*data.Field{data.NewField(f.field, nil, jsonCells(f.values))}
		}
		ff.Fields = append(values, counts)
		ff.Meta = &data.FrameMeta{Type: data.FrameTypeTable, TypeVersion: data.FrameTypeVersion{0, 1}}
		frames = append(frames, ff)
	}
	return frames, nil
}

// hintOf is the declared type of the field a column reads, if it reads one:
// a column of the driving collection's by its name, a lookup's by
// `<collection>.<field>`, `id` an int everywhere.
func hintOf(name string, schemas Schemas, shape Shape) string {
	collection, field := shape.Collection, name
	for _, l := range shape.Lookups {
		if rest, ok := strings.CutPrefix(name, l+"."); ok {
			collection, field = l, rest
			break
		}
	}
	if field == "id" {
		return "int"
	}
	return schemas[collection][field]
}

// The shapes a value's JSON can take.
type valueKind int

const (
	kNull valueKind = iota
	kBool
	kNumber
	kString
	kArray
	kObject
)

func kindOf(v json.RawMessage) valueKind {
	v = bytes.TrimSpace(v)
	if len(v) == 0 {
		return kNull
	}
	switch v[0] {
	case 'n':
		return kNull
	case 't', 'f':
		return kBool
	case '"':
		return kString
	case '[':
		return kArray
	case '{':
		return kObject
	}
	return kNumber
}

// isTimestamp is the server's form of a timestamp and no other:
// 2026-10-04T12:00:00Z, or with .123 milliseconds.
func isTimestamp(s string) bool {
	if len(s) != 20 && len(s) != 24 {
		return false
	}
	const shape = "dddd-dd-ddTdd:dd:dd"
	for i := 0; i < len(shape); i++ {
		if shape[i] == 'd' {
			if !isDigit(s[i]) {
				return false
			}
		} else if s[i] != shape[i] {
			return false
		}
	}
	if len(s) == 24 {
		return s[19] == '.' && isDigit(s[20]) && isDigit(s[21]) && isDigit(s[22]) && s[23] == 'Z'
	}
	return s[19] == 'Z'
}

// fieldsOf makes a column's fields: one of the type its values and the
// declared type agree on, two for a point (its longitude and latitude, as
// Geomap's coordinates take them), none for a vector, which a panel cannot
// draw and which can be thousands of numbers a row -- said in a notice.
func fieldsOf(name string, vals []json.RawMessage, hint string) ([]*data.Field, string) {
	kinds := map[valueKind]int{}
	for _, v := range vals {
		kinds[kindOf(v)]++
	}
	nonNull := len(vals) - kinds[kNull]
	only := func(k valueKind) bool { return kinds[k] == nonNull }

	switch {
	case strings.HasPrefix(hint, "vector<") || strings.HasPrefix(hint, "sparse<"):
		if only(kArray) || only(kString) {
			return nil, fmt.Sprintf("%s, a %s, is left out: select it as text, or leave it out of the select list", name, hint)
		}
	case hint == "geo":
		if only(kArray) {
			if lon, lat, ok := points(vals); ok {
				return []*data.Field{data.NewField(name+".lon", nil, lon), data.NewField(name+".lat", nil, lat)}, ""
			}
		}
	case hint == "timestamp":
		if only(kString) {
			if f, ok := times(name, vals); ok {
				return []*data.Field{f}, ""
			}
		}
	case hint == "text":
		if only(kString) {
			return []*data.Field{data.NewField(name, nil, strs(vals))}, ""
		}
	case hint == "json" || hint == "bytes" || strings.HasPrefix(hint, "["):
		return []*data.Field{data.NewField(name, nil, jsonCells(vals))}, ""
	}
	// Computed, or not what the schema says (an alias over a field's
	// name): the values decide.
	switch {
	case nonNull == 0:
		return []*data.Field{data.NewField(name, nil, make([]*float64, len(vals)))}, ""
	case only(kBool):
		out := make([]*bool, len(vals))
		for i, v := range vals {
			if kindOf(v) == kBool {
				b := bytes.HasPrefix(bytes.TrimSpace(v), []byte("t"))
				out[i] = &b
			}
		}
		return []*data.Field{data.NewField(name, nil, out)}, ""
	case only(kNumber):
		return []*data.Field{numbers(name, vals, hint)}, ""
	case only(kString):
		if f, ok := times(name, vals); ok {
			return []*data.Field{f}, ""
		}
		return []*data.Field{data.NewField(name, nil, strs(vals))}, ""
	}
	return []*data.Field{data.NewField(name, nil, jsonCells(vals))}, ""
}

// numbers is a column of numbers: int64s where each is a whole number the
// server wrote as one (an id past 2^53 kept exact), else float64s.
func numbers(name string, vals []json.RawMessage, hint string) *data.Field {
	ints := make([]*int64, len(vals))
	whole := hint != "float"
	for i, v := range vals {
		if kindOf(v) != kNumber {
			continue
		}
		n, err := strconv.ParseInt(string(bytes.TrimSpace(v)), 10, 64)
		if err != nil {
			whole = false
			break
		}
		ints[i] = &n
	}
	if whole {
		return data.NewField(name, nil, ints)
	}
	floats := make([]*float64, len(vals))
	for i, v := range vals {
		if kindOf(v) != kNumber {
			continue
		}
		f, err := strconv.ParseFloat(string(bytes.TrimSpace(v)), 64)
		if err != nil {
			f = math.NaN()
		}
		floats[i] = &f
	}
	return data.NewField(name, nil, floats)
}

func times(name string, vals []json.RawMessage) (*data.Field, bool) {
	out := make([]*time.Time, len(vals))
	for i, v := range vals {
		if kindOf(v) != kString {
			continue
		}
		var s string
		if json.Unmarshal(v, &s) != nil || !isTimestamp(s) {
			return nil, false
		}
		t, err := time.Parse(time.RFC3339Nano, s)
		if err != nil {
			return nil, false
		}
		t = t.UTC()
		out[i] = &t
	}
	return data.NewField(name, nil, out), true
}

func strs(vals []json.RawMessage) []*string {
	out := make([]*string, len(vals))
	for i, v := range vals {
		if kindOf(v) != kString {
			continue
		}
		var s string
		if json.Unmarshal(v, &s) == nil {
			out[i] = &s
		}
	}
	return out
}

func jsonCells(vals []json.RawMessage) []*json.RawMessage {
	out := make([]*json.RawMessage, len(vals))
	for i, v := range vals {
		if kindOf(v) == kNull {
			continue
		}
		c := append(json.RawMessage(nil), bytes.TrimSpace(v)...)
		out[i] = &c
	}
	return out
}

// points reads `[lon, lat]` values.
func points(vals []json.RawMessage) ([]*float64, []*float64, bool) {
	lon := make([]*float64, len(vals))
	lat := make([]*float64, len(vals))
	for i, v := range vals {
		if kindOf(v) == kNull {
			continue
		}
		var p []float64
		if json.Unmarshal(v, &p) != nil || len(p) != 2 {
			return nil, nil, false
		}
		lon[i], lat[i] = &p[0], &p[1]
	}
	return lon, lat, true
}

// timeSeries makes a table a time series where the query asks for one:
// format time_series, or auto with the statement grouped by its first
// column, a time -- `bucket(at, $__interval_ms) as t ... group t`. Text
// columns beside the time are labels, so a group by time and symbol
// becomes a series a symbol (long to wide), which an alert rule evaluates
// a symbol at a time.
func timeSeries(frame *data.Frame, o FrameOptions) (*data.Frame, error) {
	first := (*data.Field)(nil)
	if len(frame.Fields) > 0 {
		first = frame.Fields[0]
	}
	isTime := first != nil && first.Type().Time()
	if first == nil {
		// No rows, so no columns: an empty series, which an alert rule
		// takes for no data rather than for an error.
		if o.Format == "time_series" {
			frame.Meta.Type = data.FrameTypeTimeSeriesWide
		}
		return frame, nil
	}
	switch o.Format {
	case "time_series":
		if !isTime {
			return nil, fmt.Errorf("a time series needs a timestamp as the first column of the select list, as in `select bucket(at, $__interval_ms) as t, count(*) as n ... group t order t`")
		}
	case "table":
		return frame, nil
	default:
		g := strings.ToLower(o.Shape.GroupFirst)
		if !isTime || g == "" || (g != strings.ToLower(first.Name) && g != "bucket") {
			return frame, nil
		}
	}

	// A time series holds its time, its numbers and its labels: anything
	// else is left out and said.
	var keep []*data.Field
	var labels []int
	var dropped []string
	rows := first.Len()
	keep = append(keep, first)
	for _, f := range frame.Fields[1:] {
		switch {
		case f.Type().Numeric():
			keep = append(keep, f)
		case f.Type() == data.FieldTypeNullableString || f.Type() == data.FieldTypeNullableBool:
			labels = append(labels, len(keep))
			keep = append(keep, f)
		default:
			dropped = append(dropped, f.Name)
		}
	}
	// Rows with no time cannot be placed on the axis.
	order := make([]int, 0, rows)
	untimed := 0
	for i := 0; i < rows; i++ {
		if t, ok := first.ConcreteAt(i); ok && t != nil {
			order = append(order, i)
		} else {
			untimed++
		}
	}
	sort.SliceStable(order, func(a, b int) bool {
		ta, _ := first.ConcreteAt(order[a])
		tb, _ := first.ConcreteAt(order[b])
		return ta.(time.Time).Before(tb.(time.Time))
	})
	out := data.NewFrame(frame.Name)
	out.RefID = frame.RefID
	for fi, f := range keep {
		isLabel := false
		for _, l := range labels {
			isLabel = isLabel || l == fi
		}
		switch {
		case fi == 0:
			ts := make([]time.Time, len(order))
			for i, r := range order {
				t, _ := f.ConcreteAt(r)
				ts[i] = t.(time.Time)
			}
			out.Fields = append(out.Fields, data.NewField(f.Name, nil, ts))
		case isLabel:
			// A label is text, null the empty one.
			vs := make([]string, len(order))
			for i, r := range order {
				if v, ok := f.ConcreteAt(r); ok {
					vs[i] = fmt.Sprint(v)
				}
			}
			out.Fields = append(out.Fields, data.NewField(f.Name, nil, vs))
		default:
			nf := data.NewFieldFromFieldType(f.Type(), len(order))
			nf.Name = f.Name
			for i, r := range order {
				nf.Set(i, f.CopyAt(r))
			}
			out.Fields = append(out.Fields, nf)
		}
	}
	meta := *frame.Meta
	out.Meta = &meta
	if len(dropped) > 0 {
		out.Meta.Notices = append(out.Meta.Notices, data.Notice{Severity: data.NoticeSeverityInfo,
			Text: fmt.Sprintf("a time series holds a time, numbers and text labels: %s left out", strings.Join(dropped, ", "))})
	}
	if untimed > 0 {
		out.Meta.Notices = append(out.Meta.Notices, data.Notice{Severity: data.NoticeSeverityWarning,
			Text: fmt.Sprintf("%d rows with no time left out of the series", untimed)})
	}
	if len(out.Fields) < 2 || len(order) == 0 {
		out.Meta.Type = data.FrameTypeTimeSeriesWide
		return out, nil
	}
	if len(labels) == 0 {
		out.Meta.Type = data.FrameTypeTimeSeriesWide
		return out, nil
	}
	wide, err := data.LongToWide(out, nil)
	if err != nil {
		return nil, fmt.Errorf("a time series by %s: %w", out.Fields[0].Name, err)
	}
	wide.RefID = frame.RefID
	return wide, nil
}
