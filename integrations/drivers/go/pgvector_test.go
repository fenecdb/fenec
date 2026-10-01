// pgvector-go over fenec-server's pg wire: RegisterTypes finds `vector`,
// `halfvec` and `sparsevec` by name, and pgx then carries them in
// pgvector's binary format -- parameters, rows and CopyFrom.
package drivers

import (
	"context"
	"fmt"
	"testing"

	"github.com/jackc/pgx/v5"
	"github.com/pgvector/pgvector-go"
	pgxvec "github.com/pgvector/pgvector-go/pgx"
)

func TestPgvectorGoTypesGoBothWays(t *testing.T) {
	c, base := connect(t)
	ctx := context.Background()
	coll := base + "_pgv"
	if _, err := c.Exec(ctx, "create collection "+coll+
		" (name text, e vector<3> @hnsw(cosine), h vector<3, f16>, s sparse<5> @inverted)"); err != nil {
		t.Fatal(err)
	}
	if err := pgxvec.RegisterTypes(ctx, c); err != nil {
		t.Fatal(err)
	}
	e := pgvector.NewVector([]float32{1, 2, 3})
	h := pgvector.NewHalfVector([]float32{1.5, 2, 3})
	s := pgvector.NewSparseVector([]float32{1, 0, 0, 0.5, 0})
	if _, err := c.Exec(ctx, "put "+coll+" {name: $1, e: $2, h: $3, s: $4}", "a", e, h, s); err != nil {
		t.Fatal(err)
	}
	var (
		ge pgvector.Vector
		gh pgvector.HalfVector
		gs pgvector.SparseVector
	)
	if err := c.QueryRow(ctx, "get "+coll+" select e, h, s where name = $1", "a").Scan(&ge, &gh, &gs); err != nil {
		t.Fatal(err)
	}
	if fmt.Sprint(ge.Slice()) != "[1 2 3]" || fmt.Sprint(gh.Slice()) != "[1.5 2 3]" ||
		fmt.Sprint(gs.Indices(), gs.Values(), gs.Dimensions()) != "[0 3] [1 0.5] 5" {
		t.Fatalf("%v %v %v %v %v", ge, gh, gs.Indices(), gs.Values(), gs.Dimensions())
	}
	var hit string
	var score float64
	if err := c.QueryRow(ctx, "get "+coll+" select name near e $1 limit 1", e).Scan(&hit, &score); err != nil || hit != "a" {
		t.Fatalf("near %v %v", hit, err)
	}
	query := pgvector.NewSparseVector([]float32{1, 0, 0, 0, 0})
	if err := c.QueryRow(ctx, "get "+coll+" select name near s $1 limit 1", query).Scan(&hit, &score); err != nil || hit != "a" {
		t.Fatalf("near sparse %v %v", hit, err)
	}

	// pgvector-go's bulk load.
	rows := make([][]any, 100)
	for i := range rows {
		rows[i] = []any{fmt.Sprintf("c%d", i), pgvector.NewVector([]float32{1, float32(i), 2}), pgvector.NewHalfVector([]float32{float32(i % 8), 1, 1})}
	}
	n, err := c.CopyFrom(ctx, pgx.Identifier{coll}, []string{"name", "e", "h"}, pgx.CopyFromRows(rows))
	if err != nil || n != 100 {
		t.Fatalf("%v %v", n, err)
	}
	if err := c.QueryRow(ctx, "get "+coll+" select e, h where name = $1", "c7").Scan(&ge, &gh); err != nil ||
		fmt.Sprint(ge.Slice(), gh.Slice()) != "[1 7 2] [7 1 1]" {
		t.Fatalf("%v %v %v", ge, gh, err)
	}
}
