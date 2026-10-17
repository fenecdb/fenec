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
	"sync"
	"time"
)

// Client is fenec-server's HTTP endpoint. It is safe for use by several
// goroutines at once; After, IdempotencyKey and Wait hand out copies that
// share its connections.
type Client struct {
	root    string // the server's URL
	base    string // the URL, with /t/<tenant> when one is given
	token   string
	http    *http.Client
	timeout time.Duration
	after   uint64
	wait    time.Duration
	key     string
	tag     string
	// near: reads go to a read replica near the client where a router has
	// one (WithReadsNearest); read: this copy's requests only read (Read,
	// the builder's reads).
	near bool
	read bool
	// The furthest position an answer from home named, shared by the
	// copies: a read near is sent after it.
	pos *position
}

// position is where an answer from home left the database: its change
// (Fenec-Seq), the history that change is of (Fenec-History, <term>-<id>;
// none for a database that never had a replica) and that history's term,
// one more at each promotion.
type position struct {
	mu      sync.Mutex
	seq     uint64
	term    uint64
	history string
}

// note keeps the furthest position seen -- a later term, or further in one:
// answers arriving out of order on several goroutines must not move it back.
// note takes the position an answer from home named where it is further:
// of another lineage -- home's word on a tenant made again under the name,
// whose terms start again -- or a later term of the same, or further in
// one.
func (p *position) note(seq uint64, history string) {
	term := termOf(history)
	p.mu.Lock()
	defer p.mu.Unlock()
	if another(history, p.history) || term > p.term || (term == p.term && seq > p.seq) {
		p.seq, p.term, p.history = seq, term, history
	}
}

// told takes home's position, lower or not, which a read near's 504 names
// when no copy held the one it was sent after (sentSeq, sentHistory) -- a
// write lost in a failover: kept, every read near would wait it out for
// good. Taken only over the position sent -- a write answered meanwhile is
// further, and set back the next read near would miss it -- and never of
// an earlier term of the same lineage: a router whose copy of the
// directory lags a failover sends the read to the old primary, fenced,
// which names the branch it abandoned. Another lineage's is a tenant made
// again under the name, which starts again at term 1.
func (p *position) told(sentSeq uint64, sentHistory string, seq uint64, history string) {
	term := termOf(history)
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.seq == sentSeq && p.history == sentHistory &&
		(term >= termOf(sentHistory) || another(history, sentHistory)) {
		p.seq, p.term, p.history = seq, term, history
	}
}

func (p *position) get() (uint64, string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.seq, p.history
}

func termOf(history string) uint64 {
	term, _, _ := strings.Cut(history, "-")
	n, _ := strconv.ParseUint(term, 10, 64)
	return n
}

// lineageOf is a history's lineage: its id's top half, which every fork of
// the tenant keeps and a tenant made again under its name does not.
func lineageOf(history string) string {
	_, id, ok := strings.Cut(history, "-")
	if !ok || len(id) < 8 {
		return ""
	}
	return id[:8]
}

// another is whether history a is of another lineage than b.
func another(a, b string) bool {
	la, lb := lineageOf(a), lineageOf(b)
	return la != "" && lb != "" && la != lb
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

// WithReadsNearest marks the builder's reads, and a Read copy's requests,
// for a read replica near the client (Fenec-Read: replica): a fenec-shard
// router near a region holding the tenant's read replica answers them
// there, the rest go to the tenant's home. Each is sent after the furthest
// position an answer from home named to this client -- the change and its
// history (Fenec-After, Fenec-History) -- so it reads nothing older than it
// wrote, nor a write a failover replaced; one no copy holds that for --
// the write lost in a failover of home -- is asked again at home without
// it, and home's position, which its 504 names, is the client's from then.
func WithReadsNearest() Option { return func(c *Client) { c.near = true } }

// New is a client for the server at url, such as http://127.0.0.1:8080.
func New(url string, opts ...Option) *Client {
	root := strings.TrimRight(url, "/")
	c := &Client{
		root:    root,
		base:    root,
		http:    &http.Client{},
		timeout: 30 * time.Second,
		pos:     &position{},
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

// Read is a copy of the client whose requests only read: on a client made
// WithReadsNearest a router near the tenant's read replica answers them
// there. The builder's reads are sent so on their own; a statement the
// caller knows reads goes through Read -- one that writes is refused (403),
// never landed.
func (c *Client) Read() *Client {
	d := *c
	d.read = true
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

// Wait is a copy of the client whose set and del statements -- Exec, a
// Batch's, the builder's Update and Delete -- the server holds while they
// write nothing, until they can or for d, at most 30 seconds (Fenec-Wait):
// a worker's claim waits for a job there -- one enqueued, a delayed one
// come due, a lease lapsed -- rather than sleeping and sending it again.
// The request's context bounds it as ever; WithTimeout's bound is d longer.
// Beside After, it is also how long a read waits for the change, five
// seconds unless given.
//
//	claimed, err := db.Wait(30*time.Second).From("jobs").
//		WhereCond(Raw("run_at <= now()")).Order("run_at", "asc").Limit(10).
//		Update(ctx, D("owner", me, "run_at", Expr("now() + ?", 30000)), Returning("*"))
func (c *Client) Wait(d time.Duration) *Client {
	e := *c
	e.wait = d
	return &e
}

// IfNoneMatch is a copy of the client whose reads carry the ETag an earlier
// answer to the same statement and parameters gave: QueryRaw then hands
// back NotModified, with no body, while no write has touched what the
// statement reads -- unscoped, the server does not run it. The tag names
// a change, not the statement, so it is sent only with the request it
// came from.
func (c *Client) IfNoneMatch(tag string) *Client {
	d := *c
	d.tag = tag
	return &d
}

// Seq is the furthest change an answer from home -- a write's, a read-only
// Batch's -- left the database at, through this client or a copy of it; 0
// before any.
func (c *Client) Seq() uint64 {
	seq, _ := c.pos.get()
	return seq
}

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

// RawAnswer is an answer as the server wrote it, for a reader that needs what a
// map loses: the order of a row's columns, which is the select list's.
type RawAnswer struct {
	// JSON is the body: a query's rows, {"rows", "facets"}, a write's
	// object, or a batch's {"ok", "results"}. Nil when NotModified.
	JSON []byte
	// ETag is the tag a read was answered with, when IfNoneMatch asked for
	// one: what to send IfNoneMatch next time.
	ETag string
	// NotModified says the server answered 304: the answer IfNoneMatch
	// named still stands.
	NotModified bool
	// Seq is the change a write, or a batch, left the database at.
	Seq uint64
}

// QueryRaw runs one FenecQL statement and hands back its answer as the
// server wrote it. A client made by IfNoneMatch is answered NotModified
// while the answer it names stands; with no tag, it asks for one ("0"
// names no answer), so the next request can.
func (c *Client) QueryRaw(ctx context.Context, text string, params ...any) (RawAnswer, error) {
	var body bytes.Buffer
	if err := writeBody(&body, text, params); err != nil {
		return RawAnswer{}, err
	}
	d := *c
	if d.tag == "" {
		d.tag = `"0"`
	}
	raw, h, err := d.do(ctx, http.MethodPost, "/query", "application/json", body.Bytes(), 0)
	if err != nil {
		return RawAnswer{}, err
	}
	return RawAnswer{JSON: raw, ETag: h.etag, NotModified: h.notModified, Seq: h.seq}, nil
}

// BatchRaw runs the statements as Batch does and hands back the answer as
// the server wrote it: {"ok": n, "results": [...]}, each result a
// statement's rows in their columns' order. Statements that only read are
// one snapshot, every read at the one change Seq names.
func (c *Client) BatchRaw(ctx context.Context, stmts ...Statement) (RawAnswer, error) {
	var body bytes.Buffer
	for i, s := range stmts {
		if i > 0 {
			body.WriteByte('\n')
		}
		if err := writeBody(&body, s.Query, s.Params); err != nil {
			return RawAnswer{}, err
		}
	}
	raw, h, err := c.do(ctx, http.MethodPost, "/batch", "application/x-ndjson", body.Bytes(), 0)
	if err != nil {
		return RawAnswer{}, err
	}
	return RawAnswer{JSON: raw, Seq: h.seq}, nil
}

// Result is what a write answers.
type Result struct {
	Affected int64  `json:"affected"`
	Message  string `json:"message"`
	// Rows are what a write with returning answered: the rows it wrote, an
	// update's as written and a delete's as they were, Affected their
	// number. Nil for any other write.
	Rows []Row `json:"-"`
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
	r, err := resultOf(raw)
	if err != nil {
		return Result{}, fmt.Errorf("fenecdb: a statement answered rows, not a write: use Query")
	}
	r.Seq = h.seq
	r.Replayed = h.replayed
	return r, nil
}

// resultOf reads a write's answer: {"affected": n}, {"message": ...}, or
// the rows a write with returning answered, a bare array.
func resultOf(raw []byte) (Result, error) {
	var r Result
	if t := bytes.TrimSpace(raw); len(t) > 0 && t[0] == '[' {
		rows, err := rowsOf(raw)
		if err != nil {
			return r, err
		}
		if rows == nil {
			rows = []Row{}
		}
		r.Rows, r.Affected = rows, int64(len(rows))
		return r, nil
	}
	err := json.Unmarshal(raw, &r)
	return r, err
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
	seq         uint64
	replayed    bool
	next        uint64
	hasNext     bool
	etag        string
	notModified bool
}

// do sends one request and reads its answer whole: the body of a 2xx, an
// *Error otherwise. extra lengthens the timeout by a wait the server holds
// the answer for.
func (c *Client) do(ctx context.Context, method, path, ctype string, body []byte, extra time.Duration) ([]byte, head, error) {
	if c.timeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, c.timeout+extra+c.wait)
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
	// A 304 is an answer only to a request that named one.
	if res.StatusCode == http.StatusNotModified && c.tag != "" {
		return nil, head{etag: res.Header.Get("ETag"), notModified: true}, nil
	}
	// Asked home with no mark after a read near's 504, it is answered with
	// what home holds.
	near := c.read && c.near
	if c.toldBy(req, res.StatusCode, raw) {
		home := *c
		home.read, home.after = false, 0
		return home.do(ctx, method, path, ctype, body, extra)
	}
	if res.StatusCode >= 300 {
		return nil, head{}, errorOf(res.StatusCode, raw)
	}
	var h head
	h.etag = res.Header.Get("ETag")
	if s := res.Header.Get("Fenec-Seq"); s != "" {
		h.seq, _ = strconv.ParseUint(s, 10, 64)
		// A read near's answer may be a replica's: it moves nothing.
		if !near {
			c.pos.note(h.seq, res.Header.Get("Fenec-History"))
		}
	}
	h.replayed = res.Header.Get("Idempotent-Replayed") == "true"
	if s := res.Header.Get("Fenec-Next"); s != "" {
		h.next, _ = strconv.ParseUint(s, 10, 64)
		h.hasNext = true
	}
	return raw, h, nil
}

// toldBy says whether an answer is a 504 to a request near sent after the
// client's position: no copy the router reached holds it, home either --
// the write lost in a failover of home, which says where it stands, taken
// here as the client's position from then on (one the caller named with
// After is not the client's to take). The request goes home with no mark.
func (c *Client) toldBy(req *http.Request, status int, raw []byte) bool {
	if status != http.StatusGatewayTimeout || !(c.read && c.near) || req.Header.Get("Fenec-After") == "" {
		return false
	}
	var told struct {
		Seq     *uint64 `json:"seq"`
		History string  `json:"history"`
	}
	if c.after == 0 && json.Unmarshal(raw, &told) == nil && told.Seq != nil {
		sent, _ := strconv.ParseUint(req.Header.Get("Fenec-After"), 10, 64)
		c.pos.told(sent, req.Header.Get("Fenec-History"), *told.Seq, told.History)
	}
	return true
}

func (c *Client) headers(req *http.Request) {
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	near := c.read && c.near
	if near {
		req.Header.Set("Fenec-Read", "replica")
	}
	switch {
	case c.after > 0:
		req.Header.Set("Fenec-After", strconv.FormatUint(c.after, 10))
	case near:
		if seq, history := c.pos.get(); seq > 0 {
			req.Header.Set("Fenec-After", strconv.FormatUint(seq, 10))
			if history != "" {
				req.Header.Set("Fenec-History", history)
			}
		}
	}
	if c.tag != "" {
		req.Header.Set("If-None-Match", c.tag)
	}
	if c.key != "" {
		req.Header.Set("Idempotency-Key", c.key)
	}
	if c.wait > 0 {
		req.Header.Set("Fenec-Wait", strconv.FormatInt(c.wait.Milliseconds(), 10))
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
