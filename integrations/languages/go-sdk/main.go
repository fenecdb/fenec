// Go with the fenecdb SDK. Run by ../run-tests.sh.
package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"reflect"

	fenecdb "github.com/fenecdb/fenec/integrations/go"
)

type Hit struct {
	Title string  `json:"title"`
	Score float32 `json:"_score"`
}

func main() {
	ctx := context.Background()
	db := fenecdb.New(env("FENEC_URL", "http://127.0.0.1:8080"), fenecdb.WithToken(os.Getenv("FENEC_TOKEN")))

	must(db.Exec(ctx, "create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))"))
	must(db.Exec(ctx, "put docs {title: $1, embed: $2}", "Night at the oasis", []float32{0.1, 0.2, 0.3}))
	must(db.Exec(ctx, "put docs {title: $1, embed: $2}", "Dunes", []float32{0.9, 0.1, 0.0}))

	hits, err := fenecdb.QueryAs[Hit](ctx, db, "get docs select title near embed $1 limit 5", []float32{0.1, 0.2, 0.3})
	if err != nil {
		fail("%v", err)
	}
	titles := []string{}
	for _, h := range hits {
		titles = append(titles, h.Title)
	}
	if !reflect.DeepEqual(titles, []string{"Night at the oasis", "Dunes"}) {
		fail("near answered %v", titles)
	}

	var e *fenecdb.Error
	if _, err := db.Query(ctx, "get nowhere"); !errors.As(err, &e) || e.Status != 404 {
		fail("a missing collection was answered with %v", err)
	}
	fmt.Println("go (sdk): ok")
}

func env(name, fallback string) string {
	if v := os.Getenv(name); v != "" {
		return v
	}
	return fallback
}

func must(_ fenecdb.Result, err error) {
	if err != nil {
		fail("%v", err)
	}
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", args...)
	os.Exit(1)
}
