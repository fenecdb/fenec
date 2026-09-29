// pgx over fenec-pg's pg wire: the binary format pgx asks for every type
// it knows in, its batches, and its transactions.
package drivers

import (
	"context"
	"fmt"
	"os"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
)

func connect(t *testing.T) (*pgx.Conn, string) {
	t.Helper()
	dsn := os.Getenv("FENEC_PG_URL")
	if dsn == "" {
		t.Skip("FENEC_PG_URL is not set: integrations/drivers/run-tests.sh sets it")
	}
	ctx := context.Background()
	c, err := pgx.Connect(ctx, dsn)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close(ctx) })
	name := fmt.Sprintf("gx_%d", time.Now().UnixNano())
	_, err = c.Exec(ctx, "create collection "+name+
		" (name text, n int, score float, ok bool, at timestamp, raw bytes, e vector<2> @hnsw(cosine))")
	if err != nil {
		t.Fatal(err)
	}
	return c, name
}

func TestTypedRowsComeInTheBinaryFormat(t *testing.T) {
	c, coll := connect(t)
	ctx := context.Background()
	at := time.Date(2026, 9, 28, 12, 30, 0, 0, time.UTC)
	if _, err := c.Exec(ctx, "put "+coll+" {name: $1, n: $2, score: $3, ok: $4, at: $5, raw: $6, e: $7}",
		"a", 7, 0.25, true, at, "hi", "[1,0]"); err != nil {
		t.Fatal(err)
	}
	var (
		name  string
		n     int64
		score float64
		ok    bool
		when  time.Time
		raw   []byte
		e     string
	)
	err := c.QueryRow(ctx, "get "+coll+" select name, n, score, ok, at, raw, e where n = $1", 7).
		Scan(&name, &n, &score, &ok, &when, &raw, &e)
	if err != nil {
		t.Fatal(err)
	}
	if name != "a" || n != 7 || score != 0.25 || !ok || !when.Equal(at) || string(raw) != "hi" || e != "[1,0]" {
		t.Fatalf("got %v %v %v %v %v %q %v", name, n, score, ok, when, raw, e)
	}
	var count int64
	if err := c.QueryRow(ctx, "get "+coll+" count").Scan(&count); err != nil || count != 1 {
		t.Fatalf("count %v %v", count, err)
	}
	var hit string
	var s float64
	if err := c.QueryRow(ctx, "get "+coll+" select name near e [1, 0] limit 1").Scan(&hit, &s); err != nil || hit != "a" {
		t.Fatalf("near %v %v %v", hit, s, err)
	}
}

func TestBatchesAndTransactionsLandWhole(t *testing.T) {
	c, coll := connect(t)
	ctx := context.Background()
	b := &pgx.Batch{}
	for i := 0; i < 3; i++ {
		b.Queue("put "+coll+" {name: $1, n: $2}", fmt.Sprintf("b%d", i), i)
	}
	if err := c.SendBatch(ctx, b).Close(); err != nil {
		t.Fatal(err)
	}
	tx, err := c.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec(ctx, "put "+coll+" {name: $1, n: $2}", "gone", 99); err != nil {
		t.Fatal(err)
	}
	if err := tx.Rollback(ctx); err != nil {
		t.Fatal(err)
	}
	rows, _ := c.Query(ctx, "get "+coll+" select name, n")
	got, err := pgx.CollectRows(rows, pgx.RowToMap)
	if err != nil || len(got) != 3 {
		t.Fatalf("%v %v", got, err)
	}
}

func TestCopyFromGoesInBinary(t *testing.T) {
	c, coll := connect(t)
	ctx := context.Background()
	at := time.Date(2026, 9, 28, 12, 30, 0, 0, time.UTC)
	rows := make([][]any, 1000)
	for i := range rows {
		rows[i] = []any{fmt.Sprintf("r%d", i), int64(i), float64(i) / 4, i%2 == 0, at}
	}
	n, err := c.CopyFrom(ctx, pgx.Identifier{coll}, []string{"name", "n", "score", "ok", "at"}, pgx.CopyFromRows(rows))
	if err != nil || n != 1000 {
		t.Fatalf("%v %v", n, err)
	}
	var (
		name  string
		score float64
		ok    bool
		when  time.Time
	)
	err = c.QueryRow(ctx, "get "+coll+" select name, score, ok, at where n = $1", 7).Scan(&name, &score, &ok, &when)
	if err != nil || name != "r7" || score != 1.75 || ok || !when.Equal(at) {
		t.Fatalf("%v %v %v %v %v", name, score, ok, when, err)
	}
}

// A list field is an array: pgx scans it into a slice and binds one to it,
// both in the binary format.
func TestListsAreArrays(t *testing.T) {
	c, _ := connect(t)
	ctx := context.Background()
	coll := fmt.Sprintf("ga_%d", time.Now().UnixNano())
	if _, err := c.Exec(ctx, "create collection "+coll+" (name text, tags [text], ns [int], fs [float])"); err != nil {
		t.Fatal(err)
	}
	tags := []string{"plain", `a "q"`, `b\s`, "x,y", "", "NULL"}
	if _, err := c.Exec(ctx, "put "+coll+" {name: $1, tags: $2, ns: $3, fs: $4}",
		"a", tags, []int64{1, -2, 3}, []float64{0.5, 1.25}); err != nil {
		t.Fatal(err)
	}
	var (
		gotTags []string
		ns      []int64
		fs      []float64
	)
	err := c.QueryRow(ctx, "get "+coll+" select tags, ns, fs where name = $1", "a").Scan(&gotTags, &ns, &fs)
	if err != nil {
		t.Fatal(err)
	}
	if fmt.Sprint(gotTags) != fmt.Sprint(tags) || fmt.Sprint(ns) != "[1 -2 3]" || fmt.Sprint(fs) != "[0.5 1.25]" {
		t.Fatalf("got %q %v %v", gotTags, ns, fs)
	}
	if len(gotTags) != len(tags) {
		t.Fatalf("got %d tags", len(gotTags))
	}
}
