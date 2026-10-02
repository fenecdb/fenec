// Package fenecdb is a client for fenec-server's HTTP endpoint
// (`fenec-server --http <address>`), the standard library alone.
//
//	db := fenecdb.New("http://127.0.0.1:8080", fenecdb.WithToken("secret"))
//	rows, err := db.Query(ctx, "get docs select title near embed $1 limit 5", []float32{0.1, 0.2, 0.3})
//
// A statement is one POST /query, several are one POST /batch under one
// write lock. Values go in as $1, $2, ... and never into the text. A
// []float32 goes out as the shortest decimals that read back as each f32,
// so a vector written and read again is the same to the bit.
package fenecdb

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
)

// Client is fenec-server's HTTP endpoint. It is safe for use by several
// goroutines at once; After and IdempotencyKey hand out copies that share
// its connections.
type Client struct {
	root    string // the server's URL
	base    string // the URL, with /t/<tenant> when one is given
	token   string
	http    *http.Client
	timeout time.Duration
	after   uint64
	key     string
	// The change the last write left the database at (Fenec-Seq), shared
	// by the copies: a read on a replica sent After it waits for the write.
	seq *atomic.Uint64
}

// Option sets up a Client.
type Option func(*Client)

// WithToken sends `Authorization: Bearer <token>` with every request: the
// server's --http-token, or a JSON Web Token held to its policy.
func WithToken(token string) Option { return func(c *Client) { c.token = token } }

// WithHTTPClient sends the requests through hc rather than a client of its
// own. Its Timeout, if it has one, also cuts a subscription short: the
// requests are bounded by WithTimeout through their context instead.
func WithHTTPClient(hc *http.Client) Option { return func(c *Client) { c.http = hc } }

// WithTimeout bounds each request, 30 seconds unless given; 0 is none. A
// subscription is bounded by its context alone, and Changes waits its
// wait on top.
func WithTimeout(d time.Duration) Option { return func(c *Client) { c.timeout = d } }

// WithTenant sends every request under /t/<tenant>/: one tenant of a node
// started with --dir, or of a fenec-shard router.
func WithTenant(tenant string) Option {
	return func(c *Client) { c.base += "/t/" + url.PathEscape(tenant) }
}

// New is a client for the server at url, such as http://127.0.0.1:8080.
func New(url string, opts ...Option) *Client {
	root := strings.TrimRight(url, "/")
	c := &Client{
		root:    root,
		base:    root,
		http:    &http.Client{},
		timeout: 30 * time.Second,
		seq:     new(atomic.Uint64),
	}
	for _, o := range opts {
		o(c)
	}
	return c
}

// After is a copy of the client whose requests the server answers only once
// it holds change seq -- a write's Seq on the primary, read on a replica --
// or with a 504 after five seconds. Never from before the write.
func (c *Client) After(seq uint64) *Client {
	d := *c
	d.after = seq
	return &d
}

// IdempotencyKey is a copy of the client whose writes carry the key: sent
// again after a timeout, a write is answered as the first time and not made
// twice (Result.Replayed). One key per write; the same key with another
// request is a 422.
func (c *Client) IdempotencyKey(key string) *Client {
	d := *c
	d.key = key
	return &d
}

// Seq is the change the last write through this client, or a copy of it,
// left the database at; 0 before any.
func (c *Client) Seq() uint64 { return c.seq.Load() }

// Row is one row of an answer, as encoding/json reads an object: a number
// is a float64, a list a []any, a vector a []any of float64s.
type Row = map[string]any

// Query runs one FenecQL statement and hands back its rows. A statement
// whose answer is not rows -- a write's {"affected": n}, a create's
// {"message": ...} -- comes back as one row holding it.
func (c *Client) Query(ctx context.Context, text string, params ...any) ([]Row, error) {
	raw, _, err := c.query(ctx, text, params)
	if err != nil {
		return nil, err
	}
	return rowsOf(raw)
}

// QueryAs runs one FenecQL statement and decodes its rows into T by their
// json tags: a struct, or a map. A vector field declared []float32 reads
// back as the f32s the server holds.
func QueryAs[T any](ctx context.Context, c *Client, text string, params ...any) ([]T, error) {
	raw, _, err := c.query(ctx, text, params)
	if err != nil {
		return nil, err
	}
	return rowsAs[T](raw)
}

// QueryAnswer runs one FenecQL statement and hands back its rows and, when
// it asked for them, what its facet clause counted.
func (c *Client) QueryAnswer(ctx context.Context, text string, params ...any) (Answer, error) {
	raw, _, err := c.query(ctx, text, params)
	if err != nil {
		return Answer{}, err
	}
	return answerOf(raw)
}

// Answer is a query's rows, and the counts its facet clause asked for:
// they answer for every row the query matched, not the page, so they come
// beside the rows rather than in one.
type Answer struct {
	Rows   []Row
	Facets Facets
}

// FacetCount is one value a facet found and how many rows hold it. The
// value is any JSON value, null for the rows whose field is null.
type FacetCount struct {
	Value any   `json:"value"`
	Count int64 `json:"count"`
}

// Facet is one field's counts, most first.
type Facet struct {
	Field  string
	Counts []FacetCount
}

// Facets are the counts of each field asked, in the order asked -- a
// slice, since a map would lose it; nil when none were.
type Facets []Facet

// Of is the counts of field, or nil when it was not asked.
func (f Facets) Of(field string) []FacetCount {
	for _, x := range f {
		if x.Field == field {
			return x.Counts
		}
	}
	return nil
}

// UnmarshalJSON reads the server's {"field": [{"value", "count"}, ...]}
// in the order it wrote the fields.
func (f *Facets) UnmarshalJSON(raw []byte) error {
	if string(bytes.TrimSpace(raw)) == "null" {
		*f = nil
		return nil
	}
	dec := json.NewDecoder(bytes.NewReader(raw))
	if tok, err := dec.Token(); err != nil || tok != json.Delim('{') {
		return fmt.Errorf("fenecdb: facets are not an object: %s", raw)
	}
	out := Facets{}
	for dec.More() {
		k, err := dec.Token()
		if err != nil {
			return err
		}
		var counts []FacetCount
		if err := dec.Decode(&counts); err != nil {
			return err
		}
		out = append(out, Facet{Field: k.(string), Counts: counts})
	}
	*f = out
	return nil
}

// Result is what a write answers.
type Result struct {
	Affected int64  `json:"affected"`
	Message  string `json:"message"`
	// Seq is the change the write left the database at (Fenec-Seq): what
	// After takes on a replica. 0 for an answer replayed for its key.
	Seq uint64 `json:"-"`
	// Replayed says the answer is the one kept for the IdempotencyKey.
	Replayed bool `json:"-"`
}

// Exec runs one FenecQL statement that writes -- put, insert, set, del,
// create, drop, alter -- and hands back what it did.
func (c *Client) Exec(ctx context.Context, text string, params ...any) (Result, error) {
	raw, h, err := c.query(ctx, text, params)
	if err != nil {
		return Result{}, err
	}
	var r Result
	if err := json.Unmarshal(raw, &r); err != nil {
		return Result{}, fmt.Errorf("fenecdb: a statement answered rows, not a write: use Query")
	}
	r.Seq = h.seq
	r.Replayed = h.replayed
	return r, nil
}

// Statement is one line of a Batch.
type Statement struct {
	Query  string
	Params []any
}

// Stmt is a Statement: Stmt("put docs {title: $1}", "a").
func Stmt(query string, params ...any) Statement { return Statement{query, params} }

// BatchItem is one statement's answer in a batch: a write's, or a read's
// rows -- and the counts, when it asked for facets.
type BatchItem struct {
	Affected int64  `json:"affected"`
	Message  string `json:"message"`
	Rows     []Row  `json:"rows"`
	Facets   Facets `json:"facets"`
}

// BatchResult is what a batch answers: how many statements ran, each one's
// answer, and the change the batch left the database at.
type BatchResult struct {
	OK       int         `json:"ok"`
	Results  []BatchItem `json:"results"`
	Seq      uint64      `json:"-"`
	Replayed bool        `json:"-"`
}

// Batch runs the statements in order under one write lock, as one block:
// their writes all land, or at the first error none of them do -- an
// *Error whose Completed says how many had run. A batch holding a compact
// runs each statement on its own, and what ran before an error stays.
func (c *Client) Batch(ctx context.Context, stmts ...Statement) (BatchResult, error) {
	var body bytes.Buffer
	for i, s := range stmts {
		if i > 0 {
			body.WriteByte('\n')
		}
		if err := writeBody(&body, s.Query, s.Params); err != nil {
			return BatchResult{}, err
		}
	}
	raw, h, err := c.do(ctx, http.MethodPost, "/batch", "application/x-ndjson", body.Bytes(), 0)
	if err != nil {
		return BatchResult{}, err
	}
	var r BatchResult
	if err := json.Unmarshal(raw, &r); err != nil {
		return BatchResult{}, fmt.Errorf("fenecdb: a batch answered %q", raw)
	}
	r.Seq = h.seq
	r.Replayed = h.replayed
	return r, nil
}

// Health asks GET /_health, which takes no token and no lock: nil when the
// server answers. A tenant's is its node's.
func (c *Client) Health(ctx context.Context) error {
	node := *c
	node.base = c.root
	_, _, err := node.do(ctx, http.MethodGet, "/_health", "", nil, 0)
	return err
}

func (c *Client) query(ctx context.Context, text string, params []any) ([]byte, head, error) {
	var body bytes.Buffer
	if err := writeBody(&body, text, params); err != nil {
		return nil, head{}, err
	}
	return c.do(ctx, http.MethodPost, "/query", "application/json", body.Bytes(), 0)
}

// What a response's head says besides its status.
type head struct {
	seq      uint64
	replayed bool
	next     uint64
	hasNext  bool
}

// do sends one request and reads its answer whole: the body of a 2xx, an
// *Error otherwise. extra lengthens the timeout by a wait the server holds
// the answer for.
func (c *Client) do(ctx context.Context, method, path, ctype string, body []byte, extra time.Duration) ([]byte, head, error) {
	if c.timeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, c.timeout+extra)
		defer cancel()
	}
	var rd io.Reader
	if body != nil {
		rd = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.base+path, rd)
	if err != nil {
		return nil, head{}, err
	}
	if ctype != "" {
		req.Header.Set("Content-Type", ctype)
	}
	c.headers(req)
	res, err := c.http.Do(req)
	if err != nil {
		return nil, head{}, err
	}
	defer res.Body.Close()
	raw, err := io.ReadAll(res.Body)
	if err != nil {
		return nil, head{}, err
	}
	if res.StatusCode >= 300 {
		return nil, head{}, errorOf(res.StatusCode, raw)
	}
	var h head
	if s := res.Header.Get("Fenec-Seq"); s != "" {
		h.seq, _ = strconv.ParseUint(s, 10, 64)
		c.noteSeq(h.seq)
	}
	h.replayed = res.Header.Get("Idempotent-Replayed") == "true"
	if s := res.Header.Get("Fenec-Next"); s != "" {
		h.next, _ = strconv.ParseUint(s, 10, 64)
		h.hasNext = true
	}
	return raw, h, nil
}

func (c *Client) headers(req *http.Request) {
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	if c.after > 0 {
		req.Header.Set("Fenec-After", strconv.FormatUint(c.after, 10))
	}
	if c.key != "" {
		req.Header.Set("Idempotency-Key", c.key)
	}
}

// noteSeq keeps the highest change seen: writes answered out of order on
// several goroutines must not move it back.
func (c *Client) noteSeq(n uint64) {
	for {
		old := c.seq.Load()
		if n <= old || c.seq.CompareAndSwap(old, n) {
			return
		}
	}
}

// split is an answer's rows, as their JSON, and its facets: a query that
// asked for facets answers {"rows": [...], "facets": {...}}, any other the
// bare array -- or, for a write or a create, one object, which is one row.
func split(raw []byte) (json.RawMessage, Facets, error) {
	raw = bytes.TrimSpace(raw)
	if len(raw) == 0 || raw[0] != '{' {
		return raw, nil, nil
	}
	var both struct {
		Rows   json.RawMessage `json:"rows"`
		Facets *Facets         `json:"facets"`
	}
	// Only the two keys together: an object of a write's is a row, and so
	// is one that merely holds a "rows" field of its own.
	if err := json.Unmarshal(raw, &both); err == nil && both.Facets != nil && len(both.Rows) > 0 && both.Rows[0] == '[' {
		return both.Rows, *both.Facets, nil
	}
	return append(append([]byte{'['}, raw...), ']'), nil, nil
}

func answerOf(raw []byte) (Answer, error) {
	rows, facets, err := split(raw)
	if err != nil {
		return Answer{}, err
	}
	var a Answer
	if err := json.Unmarshal(rows, &a.Rows); err != nil {
		return Answer{}, fmt.Errorf("fenecdb: the answer is not rows: %w", err)
	}
	a.Facets = facets
	return a, nil
}

func rowsOf(raw []byte) ([]Row, error) {
	a, err := answerOf(raw)
	return a.Rows, err
}

func rowsAs[T any](raw []byte) ([]T, error) {
	rows, _, err := split(raw)
	if err != nil {
		return nil, err
	}
	var out []T
	if err := json.Unmarshal(rows, &out); err != nil {
		return nil, fmt.Errorf("fenecdb: the answer is not rows: %w", err)
	}
	return out, nil
}
