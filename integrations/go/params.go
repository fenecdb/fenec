package fenecdb

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"strconv"
)

// writeBody writes {"query": ..., "params": [...]}, one line: a batch's
// lines are its statements.
func writeBody(buf *bytes.Buffer, text string, params []any) error {
	if params == nil {
		params = []any{}
	}
	exact := make([]any, len(params))
	for i, p := range params {
		exact[i] = exactly(p)
	}
	b, err := json.Marshal(struct {
		Query  string `json:"query"`
		Params []any  `json:"params"`
	}{text, exact})
	if err != nil {
		return fmt.Errorf("fenecdb: a parameter of %q: %w", text, err)
	}
	buf.Write(b)
	return nil
}

// exactly hands float32s to encoding/json as values that write themselves
// exactly, inside lists too; everything else goes as encoding/json writes
// it -- a string, a number, a bool, nil as null, a slice as a list, a map
// as a json field's object.
func exactly(v any) any {
	switch v := v.(type) {
	case float32:
		return f32(v)
	case []float32:
		return vec32(v)
	case [][]float32:
		out := make([]any, len(v))
		for i, x := range v {
			out[i] = vec32(x)
		}
		return out
	case []any:
		out := make([]any, len(v))
		for i, x := range v {
			out[i] = exactly(x)
		}
		return out
	}
	return v
}

type f32 float32

func (f f32) MarshalJSON() ([]byte, error) { return appendF32(nil, float32(f)) }

type vec32 []float32

func (v vec32) MarshalJSON() ([]byte, error) {
	if v == nil {
		return []byte("null"), nil
	}
	out := make([]byte, 0, len(v)*12+2)
	out = append(out, '[')
	for i, f := range v {
		if i > 0 {
			out = append(out, ',')
		}
		var err error
		if out, err = appendF32(out, f); err != nil {
			return nil, err
		}
	}
	return append(out, ']'), nil
}

// appendF32 writes the shortest decimal that reads back as f. The server
// reads a number as an f64 and rounds that to an f32, and one f32 --
// 7.038531e-26 -- has a shortest text whose f64 lies exactly between it and
// the next f32 and rounds away: that one goes as its f64's text, which
// rounds back to it. encoding/json writes the shortest f32 text for every
// one, and that one landed on the server as its neighbour.
func appendF32(out []byte, f float32) ([]byte, error) {
	if math.IsNaN(float64(f)) || math.IsInf(float64(f), 0) {
		return nil, fmt.Errorf("fenecdb: %v is no number JSON can carry", f)
	}
	start := len(out)
	out = strconv.AppendFloat(out, float64(f), 'g', -1, 32)
	if back, err := strconv.ParseFloat(string(out[start:]), 64); err != nil || float32(back) != f {
		out = strconv.AppendFloat(out[:start], float64(f), 'g', -1, 64)
	}
	return out, nil
}
