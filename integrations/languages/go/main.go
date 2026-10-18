// Go with net/http and encoding/json alone. Run by ../run-tests.sh.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"reflect"
)

var url = env("FENEC_URL", "http://127.0.0.1:8080")

func env(name, fallback string) string {
	if v := os.Getenv(name); v != "" {
		return v
	}
	return fallback
}

// FenecError is a statement the server refused: its status and message.
type FenecError struct {
	Status  int
	Message string `json:"error"`
}

func (e *FenecError) Error() string { return fmt.Sprintf("%d: %s", e.Status, e.Message) }

// query runs one FenecQL statement and decodes its answer into out.
func query(out any, q string, params ...any) error {
	body, _ := json.Marshal(map[string]any{"query": q, "params": params})
	req, _ := http.NewRequest("POST", url+"/query", bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	if token := os.Getenv("FENEC_TOKEN"); token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	if res.StatusCode >= 300 {
		e := &FenecError{Status: res.StatusCode}
		json.NewDecoder(res.Body).Decode(e)
		return e
	}
	if out == nil {
		return nil
	}
	return json.NewDecoder(res.Body).Decode(out)
}

func main() {
	must(query(nil, "create collection if not exists docs (title text @text, year int @sorted)"))
	must(query(nil, "put docs {title: $1, year: $2}", "Dunes", 2021))
	must(query(nil, "put docs {title: $1, year: $2}", "Night at the oasis", 2024))

	var rows []struct {
		Title string `json:"title"`
		Year  int    `json:"year"`
	}
	must(query(&rows, "get docs select title, year where year >= $1 order year desc limit $2", 2020, 5))
	got := []string{}
	for _, r := range rows {
		got = append(got, fmt.Sprintf("%s %d", r.Title, r.Year))
	}
	if !reflect.DeepEqual(got, []string{"Night at the oasis 2024", "Dunes 2021"}) {
		fail("the filtered read answered %v", got)
	}

	var hits []struct {
		Title string  `json:"title"`
		Score float32 `json:"_score"`
	}
	must(query(&hits, "get docs select title match title $1 fuzzy", "oasys"))
	if len(hits) != 1 || hits[0].Title != "Night at the oasis" || hits[0].Score <= 0 {
		fail("match answered %v", hits)
	}

	err := query(nil, "get nowhere")
	if e, ok := err.(*FenecError); !ok || e.Status != 404 {
		fail("a missing collection was answered with %v", err)
	}
	fmt.Println("go: ok")
}

func must(err error) {
	if err != nil {
		fail("%v", err)
	}
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", args...)
	os.Exit(1)
}
