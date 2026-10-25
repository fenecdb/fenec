package plugin

// Against a real fenec-server, started here from the binary `cargo build
// -p fenec-server` makes, or FENEC_SERVER: what the server's parser does
// with what the plugin binds. Skipped when there is no binary.

import (
	"bufio"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/grafana/grafana-plugin-sdk-go/backend"
	"github.com/grafana/grafana-plugin-sdk-go/data"

	fenecdb "github.com/fenecdb/fenecdb/integrations/go"
)

const (
	liveToken  = "plugin-tests"
	liveSecret = "plugin-tests-jwt-secret-of-32-bytes-at-least"
)

func liveServer(t *testing.T) string {
	t.Helper()
	bin := os.Getenv("FENEC_SERVER")
	if bin == "" {
		bin, _ = filepath.Abs("../../../../target/debug/fenec-server")
	}
	if _, err := os.Stat(bin); err != nil {
		t.Skipf("no fenec-server at %s: cargo build -p fenec-server, or FENEC_SERVER", bin)
	}
	dir := t.TempDir()
	policy := filepath.Join(dir, "policy.txt")
	os.WriteFile(policy, []byte("ticks read for grafana\n"), 0o600)
	cmd := exec.Command(bin, "--http", "127.0.0.1:0", "--http-token", liveToken,
		"--jwt-secret", liveSecret, "--policy", policy)
	stderr, _ := cmd.StderrPipe()
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { cmd.Process.Kill(); cmd.Wait() })
	lines := bufio.NewScanner(stderr)
	for lines.Scan() {
		if _, rest, ok := strings.Cut(lines.Text(), "listening on: http://"); ok {
			go func() {
				for lines.Scan() {
				}
			}()
			return "http://" + strings.Fields(rest)[0]
		}
	}
	t.Fatal("fenec-server did not start")
	return ""
}

func jwt(claims map[string]any) string {
	enc := func(v any) string {
		b, _ := json.Marshal(v)
		return base64.RawURLEncoding.EncodeToString(b)
	}
	head := enc(map[string]string{"alg": "HS256", "typ": "JWT"})
	body := enc(claims)
	mac := hmac.New(sha256.New, []byte(liveSecret))
	mac.Write([]byte(head + "." + body))
	return head + "." + body + "." + base64.RawURLEncoding.EncodeToString(mac.Sum(nil))
}

func TestLive(t *testing.T) {
	url := liveServer(t)
	ctx := context.Background()
	admin := fenecdb.New(url, fenecdb.WithToken(liveToken))
	if _, err := admin.Exec(ctx, "create collection ticks (sym text @hash, at timestamp @sorted, px float, qty int, loc geo, note text)"); err != nil {
		t.Fatal(err)
	}
	if _, err := admin.Exec(ctx, "create collection secrets (s text)"); err != nil {
		t.Fatal(err)
	}
	rows := []any{}
	for i, sym := range []string{"S01", "S02", "S01", "S02", "S03"} {
		rows = append(rows, map[string]any{"sym": sym, "at": from.Add(time.Duration(i/2) * time.Hour).UnixMilli(),
			"px": 10 + float64(i), "qty": i + 1, "loc": []float64{28.97, 41.01}, "note": "2026-10-04T00:00:00Z"})
	}
	if _, err := admin.Exec(ctx, "put ticks $1", rows); err != nil {
		t.Fatal(err)
	}
	server := instance(t, url, `{}`, liveToken)
	scoped := instance(t, url, `{}`, jwt(map[string]any{"sub": "grafana", "role": "grafana", "exp": time.Now().Add(time.Hour).Unix()}))

	count := func(d *Datasource, q Query) (int, backend.DataResponse) {
		t.Helper()
		r := ask(t, d, dataQuery("A", q)).Responses["A"]
		if r.Error != nil {
			return -1, r
		}
		return r.Frames[0].Rows(), r
	}

	// Each of these is a symbol, one value: no row holds it.
	for _, evil := range []string{
		`S01" or sym != "`, `S01\" or 1 = 1 --`, `"); del ticks where id > 0; --`,
		`S01' or '1' = '1`, "S01\n-- x\nor true", `$1`, `${sym}`, `S01"], "query": "get secrets", "params": ["`,
	} {
		for _, d := range []*Datasource{server, scoped} {
			n, r := count(d, Query{QueryText: "get ticks select sym where sym = $sym", Vars: vars("sym", evil)})
			if n != 0 {
				t.Fatalf("%q matched %d rows (%v)", evil, n, r.Error)
			}
		}
	}
	if n, r := count(scoped, Query{QueryText: "get ticks select sym where sym = $sym", Vars: vars("sym", "S01")}); n != 2 {
		t.Fatalf("S01: %d rows, %v", n, r.Error)
	}
	// A multi-value variable after `in`, a value of it trying to be more.
	if n, r := count(scoped, Query{QueryText: "get ticks select sym where sym in $syms", Vars: vars("syms", []string{"S01", "S03", `S02" or 1 = 1`})}); n != 3 {
		t.Fatalf("in: %d rows, %v", n, r.Error)
	}
	if n, _ := count(scoped, Query{QueryText: "get ticks select sym where sym in $syms", Vars: vars("syms", []string{})}); n != 0 {
		t.Fatalf("in nothing: %d rows", n)
	}

	// The select list's order, typed by the schema and the values.
	_, r := count(scoped, Query{QueryText: "get ticks select qty, note, at, sym, loc, px order at limit 1"})
	f := r.Frames[0]
	if got := names(f); !reflect.DeepEqual(got, []string{"qty", "note", "at", "sym", "loc.lon", "loc.lat", "px"}) {
		t.Fatalf("columns %v", got)
	}
	want := []data.FieldType{data.FieldTypeNullableInt64, data.FieldTypeNullableString, data.FieldTypeNullableTime,
		data.FieldTypeNullableString, data.FieldTypeNullableFloat64, data.FieldTypeNullableFloat64, data.FieldTypeNullableFloat64}
	if got := fieldTypes(f); !reflect.DeepEqual(got, want) {
		t.Fatalf("types %v", got)
	}

	// A time series a symbol, as an alert rule reads it.
	_, r = count(scoped, Query{QueryText: "get ticks select bucket(at, $__interval_ms) as t, sym, sum(qty) as volume where at >= $__from and at < $__to group t, sym order t"})
	f = r.Frames[0]
	if f.Meta.Type != data.FrameTypeTimeSeriesWide || len(f.Fields) != 4 || f.Rows() != 3 {
		t.Fatalf("series %v %v rows %d", f.Meta.Type, names(f), f.Rows())
	}

	// The server's refusals, as the panel shows them. A collection the
	// policy does not name does not exist for its token.
	refusals := []struct {
		d    *Datasource
		q    Query
		want string
	}{
		{scoped, Query{QueryText: "get ticks where sym in $1", Params: []Param{{Value: "S01"}}}, "fenec-server answered 400: query error: position 23: expected `[`, found $1"},
		{scoped, Query{QueryText: `get ticks where sym = "none" limit 1 require 1`}, "fenec-server answered 412: `get ticks` answered 0 rows, and requires 1"},
		{scoped, Query{QueryText: "get secrets"}, "not on the server (404): collection `secrets`"},
		{instance(t, url, `{}`, "wrong"), Query{QueryText: "get ticks"}, "refused the data source's token (401)"},
	}
	for _, c := range refusals {
		_, r := count(c.d, c.q)
		if r.Error == nil || !strings.Contains(r.Error.Error(), c.want) {
			t.Fatalf("%q: %v, want %q", c.q.QueryText, r.Error, c.want)
		}
	}

	// Asked again unchanged: a 304, by the change for the server's token and
	// by the answer's digest for a scoped one.
	for _, d := range []*Datasource{server, scoped} {
		q := Query{QueryText: "get ticks select sym, count(*) as n group sym order sym"}
		_, first := count(d, q)
		_, again := count(d, q)
		if again.Frames[0].Meta.Custom.(map[string]any)["fenecdb"].(map[string]any)["notModified"] != true || !framesEqual(first.Frames, again.Frames) {
			t.Fatalf("not answered from the kept answer: %v", again.Frames[0].Meta.Custom)
		}
	}

	// The health check, with each token.
	for _, d := range []*Datasource{server, scoped} {
		h, _ := d.CheckHealth(ctx, &backend.CheckHealthRequest{})
		if h.Status != backend.HealthStatusOk {
			t.Fatalf("health: %+v", h)
		}
	}
	h, _ := scoped.CheckHealth(ctx, &backend.CheckHealthRequest{})
	if h.Message != "fenec-server answers, and the token reads 1 collection(s): ticks" {
		t.Fatalf("a scoped token's health: %s", h.Message)
	}
}
