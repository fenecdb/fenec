package plugin

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/grafana/grafana-plugin-sdk-go/backend"
	"github.com/grafana/grafana-plugin-sdk-go/data"
)

// A stand-in for fenec-server that answers what each test sets and keeps
// every request it was sent, so a test holds the plugin to the bytes it
// sends: the statement, its parameters, its path and its headers.
type fake struct {
	mu     sync.Mutex
	seen   []seenRequest
	answer func(r seenRequest) (status int, headers map[string]string, body string)
}

type seenRequest struct {
	Method, Path string
	Header       http.Header
	Body         string
}

func (f *fake) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(r.Body)
	s := seenRequest{r.Method, r.URL.Path, r.Header.Clone(), string(body)}
	f.mu.Lock()
	f.seen = append(f.seen, s)
	answer := f.answer
	f.mu.Unlock()
	if s.Path == "/_health" {
		w.WriteHeader(200)
		return
	}
	if strings.HasSuffix(s.Path, "/query") && strings.Contains(s.Body, `"query":"collections"`) {
		w.Write([]byte(`[{"name":"ticks","fields":[{"name":"sym","type":"text"},{"name":"at","type":"timestamp"},{"name":"qty","type":"int"},{"name":"px","type":"float"}]}]`))
		return
	}
	status, headers, out := answer(s)
	for k, v := range headers {
		w.Header().Set(k, v)
	}
	w.WriteHeader(status)
	w.Write([]byte(out))
}

// sent is the requests to path other than the schema's `collections`.
func (f *fake) sent(path string) []seenRequest {
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []seenRequest
	for _, s := range f.seen {
		if strings.HasSuffix(s.Path, path) && !strings.Contains(s.Body, `"query":"collections"`) {
			out = append(out, s)
		}
	}
	return out
}

func newFake(t *testing.T, answer func(seenRequest) (int, map[string]string, string)) (*fake, *httptest.Server) {
	f := &fake{answer: answer}
	srv := httptest.NewServer(f)
	t.Cleanup(srv.Close)
	return f, srv
}

func instance(t *testing.T, url string, jd string, token string) *Datasource {
	t.Helper()
	inst, err := NewDatasource(context.Background(), backend.DataSourceInstanceSettings{
		Type:                    ID,
		UID:                     "test",
		Name:                    "test",
		URL:                     url,
		JSONData:                json.RawMessage(jd),
		DecryptedSecureJSONData: map[string]string{"token": token},
	})
	if err != nil {
		t.Fatal(err)
	}
	d := inst.(*Datasource)
	if d.err != nil {
		t.Fatal(d.err)
	}
	return d
}

func dataQuery(refID string, q Query) backend.DataQuery {
	raw, _ := json.Marshal(q)
	return backend.DataQuery{
		RefID:         refID,
		JSON:          raw,
		TimeRange:     backend.TimeRange{From: from, To: to},
		Interval:      time.Hour,
		MaxDataPoints: 1000,
	}
}

func ask(t *testing.T, d *Datasource, qs ...backend.DataQuery) *backend.QueryDataResponse {
	t.Helper()
	resp, err := d.QueryData(context.Background(), &backend.QueryDataRequest{Queries: qs})
	if err != nil {
		t.Fatal(err)
	}
	return resp
}

type body struct {
	Query  string `json:"query"`
	Params []any  `json:"params"`
}

func bodyOf(t *testing.T, s seenRequest) body {
	t.Helper()
	var b body
	if err := json.Unmarshal([]byte(s.Body), &b); err != nil {
		t.Fatalf("%s: %v", s.Body, err)
	}
	return b
}

// The request a dashboard's query makes: the statement with its names made
// slots, the values as JSON parameters, the token as a bearer, the tenant
// as the path's prefix -- and a variable trying to be FenecQL a parameter.
func TestTheRequestHoldsTheStatementAndItsValuesApart(t *testing.T) {
	f, srv := newFake(t, func(seenRequest) (int, map[string]string, string) {
		return 200, nil, `[]`
	})
	d := instance(t, srv.URL+"/", `{"tenant":"acme co"}`, "the-token")
	evil := `S01"], "query": "get secrets", "params": ["`
	resp := ask(t, d, dataQuery("A", Query{
		QueryText: "get ticks select sym where sym = $sym and at >= $__from and at < $__to",
		Vars:      vars("sym", evil),
	}))
	if err := resp.Responses["A"].Error; err != nil {
		t.Fatal(err)
	}
	sent := f.sent("/query")
	if len(sent) != 1 {
		t.Fatalf("%d requests", len(sent))
	}
	s := sent[0]
	if s.Method != "POST" || s.Path != "/t/acme co/query" {
		t.Fatalf("%s %s", s.Method, s.Path)
	}
	if s.Header.Get("Authorization") != "Bearer the-token" {
		t.Fatalf("authorization %q", s.Header.Get("Authorization"))
	}
	b := bodyOf(t, s)
	if b.Query != "get ticks select sym where sym = $1 and at >= $2 and at < $3" {
		t.Fatalf("query %q", b.Query)
	}
	if !reflect.DeepEqual(b.Params, []any{evil, float64(from.UnixMilli()), float64(to.UnixMilli())}) {
		t.Fatalf("params %#v", b.Params)
	}
	// A write is refused before anything is sent.
	resp = ask(t, d, dataQuery("W", Query{QueryText: `del ticks where sym = "S01"`}))
	if e := resp.Responses["W"]; e.Error == nil || !strings.Contains(e.Error.Error(), "runs reads") || e.Status != backend.StatusBadRequest {
		t.Fatalf("a write: %+v", e)
	}
	if n := len(f.sent("/query")); n != 1 {
		t.Fatalf("a refused write was sent: %d requests", n)
	}
}

// The server's refusals reach the panel with their status and message.
func TestRefusalsKeepTheServersStatusAndMessage(t *testing.T) {
	cases := []struct {
		status int
		body   string
		want   string
	}{
		{400, `{"error":"query error: position 23: expected ` + "`[`" + `, found $1"}`, "fenec-server answered 400: query error: position 23: expected `[`, found $1"},
		{412, `{"error":"unmet: ` + "`get ticks`" + ` answered 0 rows, and requires 1"}`, "fenec-server answered 412: unmet: `get ticks` answered 0 rows, and requires 1"},
		{401, `{"error":"invalid token"}`, "fenec-server refused the data source's token (401): invalid token"},
		{403, `{"error":"the token may not read secrets"}`, "outside what the data source's token may read (403): the token may not read secrets"},
		{404, `{"error":"no collection ` + "`nope`" + `"}`, "not on the server (404): no collection `nope`"},
	}
	for _, c := range cases {
		_, srv := newFake(t, func(seenRequest) (int, map[string]string, string) { return c.status, nil, c.body })
		d := instance(t, srv.URL, `{}`, "tok")
		r := ask(t, d, dataQuery("A", Query{QueryText: "get ticks limit 1 require 1"})).Responses["A"]
		if r.Error == nil || r.Error.Error() != c.want || int(r.Status) != c.status || r.ErrorSource != backend.ErrorSourceDownstream {
			t.Fatalf("%d: %+v (%v)", c.status, r, r.Error)
		}
	}
}

// After sends Fenec-After, and the data source's wait beside it alone.
func TestAfterAndItsWait(t *testing.T) {
	f, srv := newFake(t, func(seenRequest) (int, map[string]string, string) { return 200, nil, `[]` })
	d := instance(t, srv.URL, `{"waitMs": 1500}`, "tok")
	ask(t, d, dataQuery("A", Query{QueryText: "get ticks", After: "$seq", Vars: vars("seq", "42")}))
	ask(t, d, dataQuery("A", Query{QueryText: "get ticks limit 1"}))
	sent := f.sent("/query")
	if sent[0].Header.Get("Fenec-After") != "42" || sent[0].Header.Get("Fenec-Wait") != "1500" {
		t.Fatalf("after: %v", sent[0].Header)
	}
	if sent[1].Header.Get("Fenec-After") != "" || sent[1].Header.Get("Fenec-Wait") != "" {
		t.Fatalf("no after: %v", sent[1].Header)
	}
}

// A query Grafana gave up on says so, not that the server is down.
func TestACanceledQuerySaysSo(t *testing.T) {
	_, srv := newFake(t, func(seenRequest) (int, map[string]string, string) { return 200, nil, `[]` })
	d := instance(t, srv.URL, `{}`, "tok")
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	resp, _ := d.QueryData(ctx, &backend.QueryDataRequest{Queries: []backend.DataQuery{dataQuery("A", Query{QueryText: "get ticks"})}})
	if e := resp.Responses["A"].Error; e == nil || e.Error() != "the query was canceled before fenec-server answered" {
		t.Fatalf("%v", e)
	}
}

// The same statement and parameters asked again go out with the tag of the
// last answer; a 304 is answered from the answer kept.
func TestA304IsAnsweredFromTheAnswerKept(t *testing.T) {
	writes := 0
	f, srv := newFake(t, func(s seenRequest) (int, map[string]string, string) {
		tag := `"7"`
		if writes > 0 {
			tag = `"9"`
		}
		if s.Header.Get("If-None-Match") == tag {
			return 304, map[string]string{"ETag": tag}, ""
		}
		return 200, map[string]string{"ETag": tag}, `[{"sym":"S01","n":3}]`
	})
	d := instance(t, srv.URL, `{}`, "tok")
	q := dataQuery("A", Query{QueryText: "get ticks select sym, count(*) as n group sym"})
	first := ask(t, d, q).Responses["A"]
	second := ask(t, d, q).Responses["A"]
	sent := f.sent("/query")
	if len(sent) != 2 || sent[0].Header.Get("If-None-Match") != `"0"` || sent[1].Header.Get("If-None-Match") != `"7"` {
		t.Fatalf("tags sent: %q %q", sent[0].Header.Get("If-None-Match"), sent[1].Header.Get("If-None-Match"))
	}
	if second.Error != nil || !framesEqual(first.Frames, second.Frames) {
		t.Fatalf("the kept answer differs: %v", second.Error)
	}
	custom := second.Frames[0].Meta.Custom.(map[string]any)["fenecdb"].(map[string]any)
	if custom["notModified"] != true {
		t.Fatalf("meta %v", custom)
	}
	// Another statement does not take its tag.
	ask(t, d, dataQuery("A", Query{QueryText: "get ticks select sym, count(*) as n group sym limit 1"}))
	if got := f.sent("/query")[2].Header.Get("If-None-Match"); got != `"0"` {
		t.Fatalf("another statement sent %q", got)
	}
	// After a write the tag no longer matches, and the new answer is kept.
	writes++
	third := ask(t, d, q).Responses["A"]
	if third.Frames[0].Meta.Custom.(map[string]any)["fenecdb"].(map[string]any)["notModified"] != false {
		t.Fatal("a changed answer was taken for the kept one")
	}
	ask(t, d, q)
	if got := f.sent("/query")[4].Header.Get("If-None-Match"); got != `"9"` {
		t.Fatalf("the new tag was not kept: %q", got)
	}
}

func framesEqual(a, b data.Frames) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		x, _ := a[i].MarshalJSON()
		y, _ := b[i].MarshalJSON()
		// The meta differs by its notModified alone.
		ax, bx := *a[i], *b[i]
		ax.Meta, bx.Meta = nil, nil
		x, _ = ax.MarshalJSON()
		y, _ = bx.MarshalJSON()
		if string(x) != string(y) {
			return false
		}
	}
	return true
}

// A panel's several queries are one /batch, one snapshot; one that stops
// it gets its refusal and the others are asked on their own.
func TestSeveralQueriesAreOneBatch(t *testing.T) {
	failAt := -1
	f, srv := newFake(t, func(s seenRequest) (int, map[string]string, string) {
		if s.Path == "/batch" {
			if failAt >= 0 {
				return 412, nil, `{"error":"unmet: ` + "`get ticks`" + ` answered 0 rows, and requires 1","completed":0,"at":1}`
			}
			return 200, map[string]string{"Fenec-Seq": "12"}, `{"ok":2,"results":[{"rows":[{"b":1,"a":"x"}]},{"rows":[{"n":2}]}]}`
		}
		return 200, nil, `[{"n":5}]`
	})
	d := instance(t, srv.URL, `{}`, "tok")
	qa := dataQuery("A", Query{QueryText: "get ticks select b, a"})
	qb := dataQuery("B", Query{QueryText: "get ticks select count(*) as n where at >= $__from"})
	resp := ask(t, d, qa, qb)
	batches := f.sent("/batch")
	if len(batches) != 1 {
		t.Fatalf("%d batches", len(batches))
	}
	lines := strings.Split(batches[0].Body, "\n")
	if len(lines) != 2 || bodyOf(t, seenRequest{Body: lines[1]}).Query != "get ticks select count(*) as n where at >= $1" {
		t.Fatalf("batch body %q", batches[0].Body)
	}
	if got := names(resp.Responses["A"].Frames[0]); !reflect.DeepEqual(got, []string{"b", "a"}) {
		t.Fatalf("A's columns %v", got)
	}
	if at(resp.Responses["B"].Frames[0].Fields[0], 0) != int64(2) {
		t.Fatal("B's answer")
	}
	failAt = 1
	resp = ask(t, d, qa, qb)
	if e := resp.Responses["B"]; e.Error == nil || int(e.Status) != 412 {
		t.Fatalf("B: %+v", e)
	}
	if a := resp.Responses["A"]; a.Error != nil || at(a.Frames[0].Fields[0], 0) != int64(5) {
		t.Fatalf("A was not asked again on its own: %+v", a)
	}
}

// An alert rule's query comes from Grafana's backend: no dashboard
// variables, the rule's range and interval. A group by time and symbol
// answers a wide series a symbol, which the rule's reduce and threshold
// evaluate one instance a symbol.
func TestAnAlertRulesQuery(t *testing.T) {
	f, srv := newFake(t, func(seenRequest) (int, map[string]string, string) {
		return 200, nil, `[{"t":"2026-10-04T00:00:00Z","sym":"S01","volume":10},{"t":"2026-10-04T00:00:00Z","sym":"S02","volume":30},` +
			`{"t":"2026-10-04T00:10:00Z","sym":"S01","volume":12},{"t":"2026-10-04T00:10:00Z","sym":"S02","volume":28}]`
	})
	d := instance(t, srv.URL, `{}`, "tok")
	raw := `{"refId":"A","queryText":"get ticks select bucket(at, $__interval_ms) as t, sym, sum(qty) as volume where at >= $__from and at < $__to group t, sym order t","format":"time_series","intervalMs":600000,"maxDataPoints":43200}`
	resp := ask(t, d, backend.DataQuery{RefID: "A", JSON: json.RawMessage(raw), TimeRange: backend.TimeRange{From: from, To: to}, Interval: 10 * time.Minute, MaxDataPoints: 43200})
	r := resp.Responses["A"]
	if r.Error != nil {
		t.Fatal(r.Error)
	}
	b := bodyOf(t, f.sent("/query")[0])
	if !reflect.DeepEqual(b.Params, []any{float64(600000), float64(from.UnixMilli()), float64(to.UnixMilli())}) {
		t.Fatalf("a rule's params %v", b.Params)
	}
	fr := r.Frames[0]
	if fr.Meta.Type != data.FrameTypeTimeSeriesWide || len(fr.Fields) != 3 || fr.Fields[2].Labels["sym"] != "S02" {
		t.Fatalf("a rule's frame: %v %v", fr.Meta.Type, names(fr))
	}
	// A dashboard variable has no value in a rule, and says so.
	r = ask(t, d, dataQuery("A", Query{QueryText: "get ticks where sym = $sym"})).Responses["A"]
	if r.Error == nil || !strings.Contains(r.Error.Error(), "an alert rule has none") {
		t.Fatalf("a variable in a rule: %v", r.Error)
	}
}

// The token goes to the configured server and nowhere else: a redirect is
// an error, not a request to where it points.
func TestNoRedirectIsFollowed(t *testing.T) {
	elsewhere := 0
	other := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		elsewhere++
		w.Write([]byte(`[]`))
	}))
	defer other.Close()
	_, srv := newFake(t, func(seenRequest) (int, map[string]string, string) {
		return 307, map[string]string{"Location": other.URL + "/query"}, ""
	})
	d := instance(t, srv.URL, `{}`, "tok")
	r := ask(t, d, dataQuery("A", Query{QueryText: "get ticks"})).Responses["A"]
	if r.Error == nil || elsewhere != 0 {
		t.Fatalf("a redirect was followed: %v, %d requests elsewhere", r.Error, elsewhere)
	}
}

func TestSettings(t *testing.T) {
	for _, u := range []string{"ftp://x", "http://", "not a url", "http://user:pw@host:8080", "http://host:8080/?x=1", "http://host#f"} {
		if _, err := LoadSettings(backend.DataSourceInstanceSettings{URL: u}); err == nil {
			t.Fatalf("%q was taken", u)
		}
	}
	s, err := LoadSettings(backend.DataSourceInstanceSettings{URL: " http://fenecdb:8080/ ", JSONData: json.RawMessage(`{"tenant":" acme ","waitMs":250,"timeoutSeconds":5}`),
		DecryptedSecureJSONData: map[string]string{"token": "t"}})
	if err != nil || s.URL != "http://fenecdb:8080" || s.Tenant != "acme" || s.Wait != 250*time.Millisecond || s.Timeout != 5*time.Second || s.Token != "t" {
		t.Fatalf("%+v %v", s, err)
	}
	// A data source with a wrong URL says so on every query.
	inst, _ := NewDatasource(context.Background(), backend.DataSourceInstanceSettings{URL: "ftp://x"})
	r := ask(t, inst.(*Datasource), dataQuery("A", Query{QueryText: "get t"})).Responses["A"]
	if r.Error == nil || !strings.Contains(r.Error.Error(), "not an http(s) URL") {
		t.Fatalf("%v", r.Error)
	}
}

func TestHealth(t *testing.T) {
	_, srv := newFake(t, func(s seenRequest) (int, map[string]string, string) { return 200, nil, `[{"id":1}]` })
	d := instance(t, srv.URL, `{}`, "tok")
	h, _ := d.CheckHealth(context.Background(), &backend.CheckHealthRequest{})
	if h.Status != backend.HealthStatusOk || h.Message != "fenec-server answers, and the token reads 1 collection(s): ticks" {
		t.Fatalf("%+v", h)
	}
	refusing := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/_health" {
			return
		}
		w.WriteHeader(401)
		w.Write([]byte(`{"error":"invalid token"}`))
	}))
	defer refusing.Close()
	h, _ = instance(t, refusing.URL, `{}`, "wrong").CheckHealth(context.Background(), &backend.CheckHealthRequest{})
	if h.Status != backend.HealthStatusError || !strings.Contains(h.Message, "refused the data source's token (401)") {
		t.Fatalf("%+v", h)
	}
	gone := httptest.NewServer(http.NotFoundHandler())
	gone.Close()
	h, _ = instance(t, gone.URL, `{}`, "tok").CheckHealth(context.Background(), &backend.CheckHealthRequest{})
	if h.Status != backend.HealthStatusError || !strings.Contains(h.Message, "did not answer /_health") {
		t.Fatalf("%+v", h)
	}
}

func TestExplain(t *testing.T) {
	f, srv := newFake(t, func(s seenRequest) (int, map[string]string, string) {
		return 200, nil, `[{"plan":"filter: a full scan"},{"plan":"rows: 2"}]`
	})
	d := instance(t, srv.URL, `{}`, "tok")
	var got *backend.CallResourceResponse
	body, _ := json.Marshal(explainRequest{Query: Query{QueryText: "get ticks where at >= $__from"}, From: from.UnixMilli(), To: to.UnixMilli(), IntervalMs: 1000})
	err := d.CallResource(context.Background(), &backend.CallResourceRequest{Path: "explain", Method: "POST", Body: body},
		backend.CallResourceResponseSenderFunc(func(r *backend.CallResourceResponse) error { got = r; return nil }))
	if err != nil || got.Status != 200 {
		t.Fatalf("%v %+v", err, got)
	}
	if b := bodyOf(t, f.sent("/query")[0]); b.Query != "explain get ticks where at >= $1" {
		t.Fatalf("sent %q", b.Query)
	}
	var out struct {
		Statement string   `json:"statement"`
		Plan      []string `json:"plan"`
	}
	json.Unmarshal(got.Body, &out)
	if !reflect.DeepEqual(out.Plan, []string{"filter: a full scan", "rows: 2"}) || !strings.HasPrefix(out.Statement, "get ticks where at >= $1\n-- $1 = ") {
		t.Fatalf("%s", got.Body)
	}
}
