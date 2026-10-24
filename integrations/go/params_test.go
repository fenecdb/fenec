package fenecdb

import (
	"math"
	"strconv"
	"testing"
	"time"
)

// A quota's wait is read to the millisecond where the body says it, over
// the whole seconds Retry-After rounds it up to, and in seconds where not.
func TestARefusalsWaitIsReadToTheMillisecond(t *testing.T) {
	e := errorOf(429, []byte(`{"error":"past","quota":"writes","retry_after":1,"retry_after_ms":2}`))
	if e.Quota != QuotaWrites || e.RetryAfter != 2*time.Millisecond {
		t.Fatalf("%+v", e)
	}
	e = errorOf(503, []byte(`{"error":"the tenant is being moved; retry shortly","retry_after":1}`))
	if e.RetryAfter != time.Second {
		t.Fatalf("%+v", e)
	}
}

// A write a node answered after its lease lapsed is told apart from the
// server's own error by its body's outcome, with where it left the
// database; a 500 that says none is http_500 as before.
func TestAWriteWhoseOutcomeIsUnknownSaysSo(t *testing.T) {
	e := errorOf(500, []byte(`{"error":"the write landed","outcome":"unknown","seq":7,"history":"1-00000000000000ab","completed":2,"at":3}`))
	if e.Code != CodeOutcomeUnknown || e.Seq != 7 || e.History != "1-00000000000000ab" || e.Completed != 2 || e.At != 3 {
		t.Fatalf("%+v", e)
	}
	e = errorOf(500, []byte(`{"error":"sync error: disk full"}`))
	if e.Code != "http_500" || e.Seq != 0 {
		t.Fatalf("%+v", e)
	}
}

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
