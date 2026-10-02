package fenecdb

import (
	"math"
	"strconv"
	"testing"
)

// Every f32 written reads back through an f64, as the server reads it --
// one in 4 099 of them, a spread over every exponent, and the tie.
func TestEveryF32ReadsBackThroughAnF64(t *testing.T) {
	check := func(f float32) {
		out, err := appendF32(nil, f)
		if err != nil {
			t.Fatal(err)
		}
		back, err := strconv.ParseFloat(string(out), 64)
		if err != nil || math.Float32bits(float32(back)) != math.Float32bits(f) {
			t.Fatalf("%08x went as %s and came back as %08x", math.Float32bits(f), out, math.Float32bits(float32(back)))
		}
	}
	tie := math.Float32frombits(0x15ae43fd)
	check(tie)
	check(-tie)
	if out, _ := appendF32(nil, 0.1); string(out) != "0.1" {
		t.Fatalf("0.1 went as %s", out)
	}
	for bits := uint64(0); bits <= math.MaxUint32; bits += 4099 {
		f := math.Float32frombits(uint32(bits))
		if !math.IsNaN(float64(f)) && !math.IsInf(float64(f), 0) {
			check(f)
		}
	}
}
