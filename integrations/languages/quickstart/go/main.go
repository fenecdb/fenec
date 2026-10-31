package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"strings"

	fenecdb "github.com/fenecdb/fenecdb/integrations/go"
)

type Note struct {
	Title string `json:"title"`
	Year  int    `json:"year"`
}

func titles(notes []Note) string {
	var out []string
	for _, n := range notes {
		out = append(out, n.Title)
	}
	return strings.Join(out, ", ")
}

func main() {
	ctx := context.Background()
	db := fenecdb.New("http://127.0.0.1:8080")

	if _, err := db.Exec(ctx, "put notes {title: $1, year: $2, kind: $3}", "Sand and stars", 2023, "book"); err != nil {
		log.Fatal(err)
	}

	rows, err := fenecdb.QueryAs[Note](ctx, db, "get notes where year >= $1 order year desc", 2020)
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println("from 2020:", titles(rows))

	hits, err := fenecdb.QueryAs[Note](ctx, db, "get notes select title match title $1 fuzzy", "oasys")
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println("search:", titles(hits))

	var refused *fenecdb.Error
	if _, err := db.Query(ctx, "get nowhere"); errors.As(err, &refused) {
		fmt.Println("refused:", refused.Status, refused.Message)
	}
}
