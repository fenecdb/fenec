package fenecdb

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"testing"
)

// A read near's 504 sets the client's position back only over the one the
// read was sent after -- a write answered meanwhile is further, and set back
// the next read near would miss it -- and never to an earlier term, which a
// fenced old primary names.
func TestA504SetsThePositionBackOverItsOwnAloneNeverToAnEarlierTerm(t *testing.T) {
	const h1, h2 = "1-00000000000000aa", "2-00000000000000bb"
	var mu sync.Mutex
	var marked []http.Header
	writes, reads := 0, 0
	arrived, release := make(chan struct{}), make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		mu.Lock()
		switch {
		case strings.Contains(string(body), "put"):
			writes++
			w.Header().Set("Fenec-Seq", strconv.Itoa(99+writes))
			w.Header().Set("Fenec-History", h2)
			mu.Unlock()
			_, _ = w.Write([]byte(`{"affected":1}`))
		case r.Header.Get("Fenec-Read") != "":
			marked = append(marked, r.Header.Clone())
			reads++
			n := reads
			mu.Unlock()
			switch n {
			case 1:
				close(arrived)
				<-release
				w.WriteHeader(http.StatusGatewayTimeout)
				_, _ = w.Write([]byte(`{"error":"x","seq":100,"history":"` + h2 + `"}`))
			case 3:
				w.WriteHeader(http.StatusGatewayTimeout)
				_, _ = w.Write([]byte(`{"error":"x","seq":140,"history":"` + h1 + `"}`))
			default:
				_, _ = w.Write([]byte(`[]`))
			}
		default:
			mu.Unlock()
			_, _ = w.Write([]byte(`[]`))
		}
	}))
	defer srv.Close()
	ctx := context.Background()
	db := New(srv.URL, WithReadsNearest())
	if _, err := db.Exec(ctx, "put t {n: 1}"); err != nil {
		t.Fatal(err)
	}
	done := make(chan error)
	go func() {
		_, err := db.From("t").Rows(ctx)
		done <- err
	}()
	<-arrived
	if _, err := db.Exec(ctx, "put t {n: 2}"); err != nil {
		t.Fatal(err)
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 3; i++ {
		if _, err := db.From("t").Rows(ctx); err != nil {
			t.Fatal(err)
		}
	}
	mu.Lock()
	defer mu.Unlock()
	if len(marked) != 4 {
		t.Fatalf("%d marked reads", len(marked))
	}
	for _, i := range []int{1, 3} {
		if h := marked[i]; h.Get("Fenec-After") != "101" || h.Get("Fenec-History") != h2 {
			t.Fatalf("read %d: %v", i, h)
		}
	}
}

// A client made WithReadsNearest marks the builder's reads and a Read
// copy's requests for a read replica near it (Fenec-Read: replica), each
// after the furthest position an answer from home named (Fenec-After,
// Fenec-History); its writes, and every request of a client reading home,
// go as before. A read no copy holds that for (504) is asked home with
// neither, and home's position, which the 504 names, is the client's from
// then. Against a scripted server: what a router does with the mark is
// fenec-shard's to test, and the JavaScript client's against real routers.
func TestReadsNearestAreMarkedAndSentAfterTheClientsWrites(t *testing.T) {
	type answer struct {
		status  int
		body    string
		seq     string
		history string
	}
	const h1, h2 = "1-00000000000000aa", "2-00000000000000bb"
	script := []answer{
		{200, `{"affected":1}`, "9", h1},
		{200, `{"affected":1}`, "12", h1},
		{200, `{"affected":1}`, "10", h1},
		{200, `[{"n":1}]`, "", ""},
		{200, `[{"count":2}]`, "", ""},
		{200, `[{"count":2}]`, "", ""},
		// A read near's answer moves nothing: it may be a replica's.
		{200, `{"ok":1,"results":[]}`, "15", h1},
		{504, `{"error":"change 12 is not in this copy's history","seq":10,"history":"` + h2 + `"}`, "", ""},
		{200, `[{"n":1}]`, "", ""},
		{200, `[{"n":1}]`, "", ""},
	}
	var mu sync.Mutex
	var seen []http.Header
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		seen = append(seen, r.Header.Clone())
		a := script[0]
		script = script[1:]
		if a.seq != "" {
			w.Header().Set("Fenec-Seq", a.seq)
			w.Header().Set("Fenec-History", a.history)
		}
		w.WriteHeader(a.status)
		_, _ = w.Write([]byte(a.body))
	}))
	defer srv.Close()
	ctx := context.Background()
	db := New(srv.URL, WithReadsNearest())
	for _, text := range []string{`put t {n: 1}`, `put t {n: 2}`, `put t {n: 3}`} {
		if _, err := db.IdempotencyKey(text).Exec(ctx, text); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := db.From("t").Rows(ctx); err != nil {
		t.Fatal(err)
	}
	if n, err := db.From("t").Count(ctx); err != nil || n != 2 {
		t.Fatalf("count %d %v", n, err)
	}
	if _, err := db.Query(ctx, "get t count"); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Read().Batch(ctx, Stmt("get t count")); err != nil {
		t.Fatal(err)
	}
	rows, err := db.From("t").Rows(ctx)
	if err != nil || len(rows) != 1 {
		t.Fatalf("rows %v %v", rows, err)
	}
	if _, err := db.From("t").Rows(ctx); err != nil {
		t.Fatal(err)
	}
	if len(seen) != 10 {
		t.Fatalf("%d requests", len(seen))
	}
	for i, h := range seen[:3] {
		if h.Get("Fenec-Read") != "" || h.Get("Fenec-After") != "" {
			t.Fatalf("write %d marked: %v", i, h)
		}
	}
	// The furthest position, 12, not the last answered, 10.
	for _, i := range []int{3, 4, 6, 7} {
		h := seen[i]
		if h.Get("Fenec-Read") != "replica" || h.Get("Fenec-After") != "12" || h.Get("Fenec-History") != h1 {
			t.Fatalf("read %d: %v", i, h)
		}
	}
	for _, i := range []int{5, 8} {
		if seen[i].Get("Fenec-Read") != "" || seen[i].Get("Fenec-After") != "" || seen[i].Get("Fenec-History") != "" {
			t.Fatalf("request %d goes home: %v", i, seen[i])
		}
	}
	// Home's position taken, lower and of the history after the failover.
	if h := seen[9]; h.Get("Fenec-After") != "10" || h.Get("Fenec-History") != h2 {
		t.Fatalf("after the 504: %v", h)
	}
	if db.Seq() != 10 {
		t.Fatalf("seq %d", db.Seq())
	}
}

func TestAClientReadsHomeUnlessItAsksForNearest(t *testing.T) {
	var seen []http.Header
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen = append(seen, r.Header.Clone())
		_, _ = w.Write([]byte(`[]`))
	}))
	defer srv.Close()
	db := New(srv.URL)
	if _, err := db.From("t").Rows(context.Background()); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Read().Query(context.Background(), "get t count"); err != nil {
		t.Fatal(err)
	}
	for _, h := range seen {
		if h.Get("Fenec-Read") != "" {
			t.Fatalf("marked: %v", h)
		}
	}
}

// A tenant made again under its name starts again at term 1, of another
// lineage -- the top half of its history's id: home's position is taken,
// from a 504 and from an answer, its term lower. Refused as an earlier
// term, every read near waited its whole wait for good. Within one lineage
// an earlier term is still refused.
func TestATenantMadeAgainHasItsPositionTakenItsTermLower(t *testing.T) {
	const old, remade, third = "3-aaaaaaaa000000c3", "1-bbbbbbbb00000001", "1-cccccccc00000001"
	const second = "2-cccccccc00000002"
	var p position
	p.note(50, old)
	p.told(50, old, 2, remade)
	if seq, h := p.get(); seq != 2 || h != remade {
		t.Fatalf("after the 504: %d %s", seq, h)
	}
	p.note(1, third)
	if seq, h := p.get(); seq != 1 || h != third {
		t.Fatalf("after an answer: %d %s", seq, h)
	}
	p.note(9, second)
	p.note(90, third)
	if seq, h := p.get(); seq != 9 || h != second {
		t.Fatalf("an earlier term of the same lineage taken: %d %s", seq, h)
	}
}
