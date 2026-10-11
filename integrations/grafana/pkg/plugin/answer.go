package plugin

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// An answer as fenec-server wrote it, in its order: encoding/json's maps
// lose the order of a row's keys, which is the select list's, and a panel
// shows its columns in that order -- Infinity's backend parser sorted them
// by name, and every panel needed Grafana's Organize transformation to
// put them back.

// cell is one column of a row: its key and its value's JSON.
type cell struct {
	key string
	val json.RawMessage
}

// row is a row's cells in the order the server wrote them.
type row []cell

func (r row) get(key string) (json.RawMessage, bool) {
	for _, c := range r {
		if c.key == key {
			return c.val, true
		}
	}
	return nil, false
}

// facetCounts is one field's facet: each value and how many rows hold it.
type facetCounts struct {
	field  string
	values []json.RawMessage
	counts []int64
}

// answer is a read's rows and, when it asked for them, its facets.
type answer struct {
	rows   []row
	facets []facetCounts
}

// parseAnswer reads /query's answer to a read: a bare array of rows, or
// {"rows": [...], "facets": {...}} when the statement asked for facets.
func parseAnswer(raw []byte) (answer, error) {
	raw = bytes.TrimSpace(raw)
	if len(raw) == 0 {
		return answer{}, fmt.Errorf("fenec-server answered nothing")
	}
	if raw[0] == '[' {
		rows, err := parseRows(raw)
		return answer{rows: rows}, err
	}
	obj, err := parseObject(raw)
	if err != nil {
		return answer{}, err
	}
	rowsRaw, hasRows := obj.get("rows")
	facetsRaw, hasFacets := obj.get("facets")
	if !hasRows || !hasFacets || len(obj) != 2 {
		// A write's {"affected": n} or a create's {"message": ...}: the
		// data source sends reads alone, so this is no answer it reads.
		return answer{}, fmt.Errorf("the statement answered no rows: %s", clip(raw))
	}
	rows, err := parseRows(rowsRaw)
	if err != nil {
		return answer{}, err
	}
	facets, err := parseFacets(facetsRaw)
	return answer{rows: rows, facets: facets}, err
}

// parseBatch reads /batch's {"ok": n, "results": [...]}: each result an
// object holding "rows" and, where asked, "facets".
func parseBatch(raw []byte) ([]answer, error) {
	obj, err := parseObject(raw)
	if err != nil {
		return nil, err
	}
	results, ok := obj.get("results")
	if !ok {
		return nil, fmt.Errorf("a batch answered no results: %s", clip(raw))
	}
	dec := json.NewDecoder(bytes.NewReader(results))
	if err := expectDelim(dec, '['); err != nil {
		return nil, err
	}
	var out []answer
	for dec.More() {
		var item json.RawMessage
		if err := dec.Decode(&item); err != nil {
			return nil, err
		}
		r, err := parseObject(item)
		if err != nil {
			return nil, err
		}
		var a answer
		rowsRaw, ok := r.get("rows")
		if !ok {
			return nil, fmt.Errorf("a batch's statement answered no rows: %s", clip(item))
		}
		if a.rows, err = parseRows(rowsRaw); err != nil {
			return nil, err
		}
		if f, ok := r.get("facets"); ok {
			if a.facets, err = parseFacets(f); err != nil {
				return nil, err
			}
		}
		out = append(out, a)
	}
	return out, nil
}

func clip(raw []byte) string {
	if len(raw) > 200 {
		return string(raw[:200]) + "..."
	}
	return string(raw)
}

func expectDelim(dec *json.Decoder, want json.Delim) error {
	tok, err := dec.Token()
	if err != nil {
		return err
	}
	if d, ok := tok.(json.Delim); !ok || d != want {
		return fmt.Errorf("expected %q in the answer, found %v", want, tok)
	}
	return nil
}

// parseObject reads one JSON object's members in their order.
func parseObject(raw []byte) (row, error) {
	dec := json.NewDecoder(bytes.NewReader(raw))
	if err := expectDelim(dec, '{'); err != nil {
		return nil, err
	}
	var out row
	for dec.More() {
		k, err := dec.Token()
		if err != nil {
			return nil, err
		}
		key, ok := k.(string)
		if !ok {
			return nil, fmt.Errorf("an object's key is not a string: %v", k)
		}
		var v json.RawMessage
		if err := dec.Decode(&v); err != nil {
			return nil, err
		}
		out = append(out, cell{key, v})
	}
	return out, nil
}

// parseRows reads an array of row objects.
func parseRows(raw []byte) ([]row, error) {
	dec := json.NewDecoder(bytes.NewReader(raw))
	if err := expectDelim(dec, '['); err != nil {
		return nil, err
	}
	var out []row
	for dec.More() {
		var item json.RawMessage
		if err := dec.Decode(&item); err != nil {
			return nil, err
		}
		r, err := parseObject(item)
		if err != nil {
			return nil, fmt.Errorf("a row is not an object: %w", err)
		}
		out = append(out, r)
	}
	return out, nil
}

// parseFacets reads {"field": [{"value": v, "count": n}, ...], ...} in
// the order the facets were asked.
func parseFacets(raw []byte) ([]facetCounts, error) {
	obj, err := parseObject(raw)
	if err != nil {
		return nil, err
	}
	out := make([]facetCounts, 0, len(obj))
	for _, c := range obj {
		var counts []struct {
			Value json.RawMessage `json:"value"`
			Count int64           `json:"count"`
		}
		if err := json.Unmarshal(c.val, &counts); err != nil {
			return nil, fmt.Errorf("the facet %s: %w", c.key, err)
		}
		f := facetCounts{field: c.key}
		for _, x := range counts {
			f.values = append(f.values, x.Value)
			f.counts = append(f.counts, x.Count)
		}
		out = append(out, f)
	}
	return out, nil
}

// table is an answer's rows as columns: the names in order, and a value
// each row, nil where a row has none.
type table struct {
	names []string
	cols  [][]json.RawMessage
}

// flatten lays the rows out as one table. A `lookup` nests each parent's
// children inside it, under the looked-up collection's name; a table has
// no nesting, so it gets what ResultSet::flatten gives every flat
// transport (crates/fenec-core/src/query.rs): the parent's columns, then
// each level's under `<collection>.<field>`, one row per root-to-leaf
// path, and nulls from the first level that ran out.
func flatten(rows []row, lookups []string) table {
	levels := len(lookups)
	// The columns each level holds, in the order first seen: a row of one
	// level holds the same keys as the next, and `_score` only where a
	// search scored it.
	names := make([][]string, levels+1)
	seen := make([]map[string]bool, levels+1)
	for i := range seen {
		seen[i] = map[string]bool{}
	}
	var collect func(rs []row, depth int)
	collect = func(rs []row, depth int) {
		for _, r := range rs {
			for _, c := range r {
				if depth < levels && c.key == lookups[depth] {
					if children, err := parseRows(c.val); err == nil {
						collect(children, depth+1)
					}
					continue
				}
				if !seen[depth][c.key] {
					seen[depth][c.key] = true
					names[depth] = append(names[depth], c.key)
				}
			}
		}
	}
	collect(rows, 0)

	var t table
	offsets := make([]int, levels+1)
	for d, ns := range names {
		offsets[d] = len(t.names)
		for _, n := range ns {
			if d == 0 {
				t.names = append(t.names, n)
			} else {
				t.names = append(t.names, lookups[d-1]+"."+n)
			}
		}
	}
	t.cols = make([][]json.RawMessage, len(t.names))
	path := make([]json.RawMessage, len(t.names))
	emit := func() {
		for i := range t.cols {
			t.cols[i] = append(t.cols[i], path[i])
		}
	}
	var walk func(r row, depth int)
	walk = func(r row, depth int) {
		for i, n := range names[depth] {
			v, _ := r.get(n)
			path[offsets[depth]+i] = v
		}
		if depth == levels {
			emit()
			return
		}
		var children []row
		if raw, ok := r.get(lookups[depth]); ok {
			children, _ = parseRows(raw)
		}
		if len(children) == 0 {
			// This level ran out: its block and every block below null.
			for i := offsets[depth+1]; i < len(path); i++ {
				path[i] = nil
			}
			emit()
			return
		}
		for _, c := range children {
			walk(c, depth+1)
		}
	}
	for _, r := range rows {
		walk(r, 0)
	}
	return t
}
