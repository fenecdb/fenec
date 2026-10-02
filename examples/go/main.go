// Notes over fenec-server's HTTP endpoint, with the Go SDK.
//
//	go run .                            seeds if empty, then lists
//	go run . add <title> <body> [tag ...]
//	go run . list [--tag T] [--open]
//	go run . search <words>
//	go run . done <id>
//	go run . watch                      the open notes again after every change
//	go run . smoke                      what CI runs
package main

import (
	"context"
	_ "embed"
	"fmt"
	"hash/fnv"
	"math"
	"net/url"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	fenecdb "github.com/fenecdb/fenec/integrations/go"
)

//go:embed schema.fenecql
var schema string

type Note struct {
	ID    int64    `json:"id"`
	Title string   `json:"title"`
	Tags  []string `json:"tags"`
	Done  bool     `json:"done"`
	At    string   `json:"at"`
}

var seeds = []struct {
	title, body string
	tags        []string
	done        bool
	at          string
}{
	{"Groceries", "Buy milk, eggs and fresh bread for the weekend.", []string{"home", "shopping"}, false, "2026-09-28T09:00:00Z"},
	{"Release checklist", "Tag the release, publish the packages and update the docs.", []string{"work"}, false, "2026-09-29T09:00:00Z"},
	{"Book flights", "Find cheap flights to Istanbul for the conference in spring.", []string{"travel", "work"}, true, "2026-09-30T09:00:00Z"},
	{"Book club", "Finish the novel about the desert fox before Thursday.", []string{"home", "reading"}, false, "2026-10-01T09:00:00Z"},
}

// embed is a TOY embedding, a placeholder for a real model: hashed
// character trigrams (FNV-1a over the UTF-8 bytes) into 64 dimensions. It
// matches spelling, not meaning. A real one is an embeddings API call or a
// local model, with the field's dimension changed to match.
func embed(text string) []float32 {
	b := []byte(" " + asciiLower(text) + " ")
	v := make([]float32, 64)
	for i := 0; i+3 <= len(b); i++ {
		h := fnv.New32a()
		h.Write(b[i : i+3])
		v[h.Sum32()%64]++
	}
	var n float64
	for _, x := range v {
		n += float64(x) * float64(x)
	}
	if n > 0 {
		for i := range v {
			v[i] = float32(float64(v[i]) / math.Sqrt(n))
		}
	}
	return v
}

func asciiLower(s string) string {
	return strings.Map(func(r rune) rune {
		if r >= 'A' && r <= 'Z' {
			return r + 32
		}
		return r
	}, s)
}

func env(name, fallback string) string {
	if v := os.Getenv(name); v != "" {
		return v
	}
	return fallback
}

func add(ctx context.Context, db *fenecdb.Client, title, body string, tags []string, done bool, at string) error {
	_, err := db.From("notes").Insert(ctx, fenecdb.D(
		"title", title, "body", body, "tags", tags, "done", done, "at", at, "embed", embed(title+" "+body)))
	return err
}

func seed(ctx context.Context, db *fenecdb.Client) error {
	n, err := db.From("notes").Count(ctx)
	if err != nil || n > 0 {
		return err
	}
	for _, s := range seeds {
		if err := add(ctx, db, s.title, s.body, s.tags, s.done, s.at); err != nil {
			return err
		}
	}
	return nil
}

func list(ctx context.Context, db *fenecdb.Client, tag string, open bool) ([]Note, error) {
	q := db.From("notes").Select("id", "title", "tags", "done", "at").Order("at", "desc").Limit(20)
	if tag != "" {
		q = q.Where("tags", "has", tag)
	}
	if open {
		q = q.Where("done", "=", false)
	}
	return fenecdb.RowsAs[Note](ctx, q)
}

func titles(notes []Note) []string {
	out := []string{}
	for _, n := range notes {
		out = append(out, n.Title)
	}
	return out
}

func show(notes []Note) {
	for _, n := range notes {
		mark := " "
		if n.Done {
			mark = "x"
		}
		fmt.Printf("[%s] %3d  %-20s %s\n", mark, n.ID, n.Title, strings.Join(n.Tags, ", "))
	}
}

func search(ctx context.Context, db *fenecdb.Client, words string) (match, fused []Note, err error) {
	notes := db.From("notes").Select("id", "title")
	if match, err = fenecdb.RowsAs[Note](ctx, notes.Match("body", words).Limit(5)); err != nil {
		return
	}
	fused, err = fenecdb.RowsAs[Note](ctx, notes.Match("body", words).Near("embed", embed(words)).Fuse().Limit(5))
	return
}

// watch prints the open notes again whenever they change: a subscription
// to the collection's rows that are not done, over server-sent events.
func watch(ctx context.Context, db *fenecdb.Client) error {
	events, err := db.Subscribe(ctx, "notes", url.Values{"done": {"eq.false"}, "select": {"id"}})
	if err != nil {
		return err
	}
	for ev := range events {
		if ev.Err != nil {
			return ev.Err
		}
		open, err := list(ctx, db, "", true)
		if err != nil {
			return err
		}
		fmt.Println("--")
		show(open)
	}
	return nil
}

func smoke(ctx context.Context, db *fenecdb.Client) error {
	check := func(step string, ok bool) {
		if !ok {
			fmt.Println("FAIL", step)
			os.Exit(1)
		}
		fmt.Println("ok  ", step)
	}
	if err := seed(ctx, db); err != nil {
		return err
	}
	n, err := db.From("notes").Count(ctx)
	if err != nil {
		return err
	}
	check("seeded 4 notes", n == 4)
	var nonzero []int
	for i, x := range embed("hello") {
		if x != 0 {
			nonzero = append(nonzero, i)
		}
	}
	check("toy embedding", slices.Equal(nonzero, []int{24, 36, 46, 48, 62}))
	all, err := list(ctx, db, "", false)
	if err != nil {
		return err
	}
	check("newest first", all[0].Title == "Book club")
	work, _ := list(ctx, db, "work", false)
	check("by tag", slices.Equal(titles(work), []string{"Book flights", "Release checklist"}))
	open, _ := list(ctx, db, "", true)
	check("open", len(open) == 3)
	match, _, err := search(ctx, db, "release docs")
	if err != nil {
		return err
	}
	check("match", match[0].Title == "Release checklist")
	near, err := fenecdb.RowsAs[Note](ctx, db.From("notes").Near("embed", embed("flights to Istanbul")).Limit(1))
	if err != nil {
		return err
	}
	check("near", near[0].Title == "Book flights")
	_, fused, err := search(ctx, db, "desert fox")
	if err != nil {
		return err
	}
	check("fuse", fused[0].Title == "Book club")

	sub, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	events, err := db.Subscribe(sub, "notes", url.Values{"done": {"eq.false"}})
	if err != nil {
		return err
	}
	<-events // the seed
	if err := add(ctx, db, "Call mom", "Ask about the weekend.", []string{"home"}, false, time.Now().UTC().Format(time.RFC3339)); err != nil {
		return err
	}
	seen := false
	for ev := range events {
		if ev.Type == "change" && len(ev.Puts) > 0 && ev.Puts[0]["title"] == "Call mom" {
			seen = true
			break
		}
	}
	check("live subscription", seen)

	if _, err := db.From("notes").Where("title", "=", "Groceries").Update(ctx, map[string]any{"done": true}); err != nil {
		return err
	}
	open, _ = list(ctx, db, "", true)
	check("done", len(open) == 3)
	return nil
}

func run(ctx context.Context, args []string) error {
	db := fenecdb.New(env("FENEC_URL", "http://127.0.0.1:8080"),
		fenecdb.WithToken(env("FENEC_TOKEN", "secret"))) // a dev default; never ship one
	// Makes what is missing; refuses what would lose data.
	if _, err := db.Schema(ctx, schema, nil, true); err != nil {
		return err
	}
	cmd := ""
	if len(args) > 0 {
		cmd = args[0]
	}
	switch cmd {
	case "add":
		return add(ctx, db, args[1], args[2], args[3:], false, time.Now().UTC().Format(time.RFC3339))
	case "list":
		tag := ""
		if i := slices.Index(args, "--tag"); i >= 0 && i+1 < len(args) {
			tag = args[i+1]
		}
		notes, err := list(ctx, db, tag, slices.Contains(args, "--open"))
		show(notes)
		return err
	case "search":
		match, fused, err := search(ctx, db, strings.Join(args[1:], " "))
		fmt.Println("match:", strings.Join(titles(match), ", "))
		fmt.Println("fuse: ", strings.Join(titles(fused), ", "))
		return err
	case "done":
		id, err := strconv.ParseInt(args[1], 10, 64)
		if err != nil {
			return err
		}
		_, err = db.From("notes").Where("id", "=", id).Update(ctx, map[string]any{"done": true})
		return err
	case "watch":
		return watch(ctx, db)
	case "smoke":
		return smoke(ctx, db)
	default:
		if err := seed(ctx, db); err != nil {
			return err
		}
		notes, err := list(ctx, db, "", false)
		show(notes)
		return err
	}
}

func main() {
	if err := run(context.Background(), os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
