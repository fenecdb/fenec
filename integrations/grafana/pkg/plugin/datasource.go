// Package plugin is fenecdb-datasource's backend: it binds a panel's or an
// alert rule's FenecQL to its parameters, sends it to fenec-server over the
// Go SDK (integrations/go), and makes the answer Grafana's frames.
package plugin

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/grafana/grafana-plugin-sdk-go/backend"
	"github.com/grafana/grafana-plugin-sdk-go/backend/httpclient"
	"github.com/grafana/grafana-plugin-sdk-go/backend/instancemgmt"
	"github.com/grafana/grafana-plugin-sdk-go/backend/log"

	fenecdb "github.com/fenecdb/fenecdb/integrations/go"
)

var (
	_ backend.QueryDataHandler      = (*Datasource)(nil)
	_ backend.CheckHealthHandler    = (*Datasource)(nil)
	_ backend.CallResourceHandler   = (*Datasource)(nil)
	_ instancemgmt.InstanceDisposer = (*Datasource)(nil)
)

// Datasource is one data source's instance: its client, the answers it
// keeps for If-None-Match, and the collections' schemas it types columns
// by.
type Datasource struct {
	// err is settings that do not load: every query and the health check
	// say it, rather than Grafana's "plugin unavailable".
	err      error
	settings Settings
	client   *fenecdb.Client
	answers  *answers
	schemas  schemaCache
}

// NewDatasource makes a data source's instance from its settings. Every
// request goes to the settings' URL and a path the plugin names -- /query,
// /batch, /_health -- under /t/<tenant>/ when one is set: nothing a query
// holds becomes part of a URL, and the client follows no redirect, so the
// token is sent to that server alone.
func NewDatasource(ctx context.Context, s backend.DataSourceInstanceSettings) (instancemgmt.Instance, error) {
	st, err := LoadSettings(s)
	if err != nil {
		return &Datasource{err: err}, nil
	}
	opts, err := s.HTTPClientOptions(ctx)
	if err != nil {
		return &Datasource{err: fmt.Errorf("the data source's HTTP settings: %w", err)}, nil
	}
	// The token is the one credential sent: no basic auth, no headers of
	// the settings' own, nothing forwarded from the viewer's request.
	opts.BasicAuth = nil
	opts.Header = nil
	opts.ForwardHTTPHeaders = false
	// fenec-server closes a keep-alive connection silent for 60 s
	// (--idle-timeout), and net/http sends no POST again that met a
	// connection closing under it -- it retries idempotent methods alone --
	// so the client lets its idle ones go first.
	if opts.Timeouts != nil {
		t := *opts.Timeouts
		t.IdleConnTimeout = min(t.IdleConnTimeout, 30*time.Second)
		opts.Timeouts = &t
	}
	opts.ConfigureClient = func(_ httpclient.Options, c *http.Client) {
		c.CheckRedirect = func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }
	}
	hc, err := httpclient.New(opts)
	if err != nil {
		return &Datasource{err: fmt.Errorf("the data source's HTTP client: %w", err)}, nil
	}
	fo := []fenecdb.Option{fenecdb.WithHTTPClient(hc)}
	if st.Token != "" {
		fo = append(fo, fenecdb.WithToken(st.Token))
	}
	if st.Tenant != "" {
		fo = append(fo, fenecdb.WithTenant(st.Tenant))
	}
	if st.Timeout > 0 {
		fo = append(fo, fenecdb.WithTimeout(st.Timeout))
	}
	return &Datasource{
		settings: st,
		client:   fenecdb.New(st.URL, fo...),
		answers:  newAnswers(32 << 20),
	}, nil
}

// Dispose lets an instance go when its settings change.
func (d *Datasource) Dispose() {}

// prepared is a query bound and ready to send.
type prepared struct {
	refID  string
	format string
	bound  Bound
}

func prepare(q backend.DataQuery) (prepared, error) {
	var m Query
	if err := json.Unmarshal(q.JSON, &m); err != nil {
		return prepared{}, fmt.Errorf("the query's JSON: %w", err)
	}
	b, err := Bind(m, Builtins{From: q.TimeRange.From, To: q.TimeRange.To, Interval: q.Interval, MaxDataPoints: q.MaxDataPoints})
	if err != nil {
		return prepared{}, err
	}
	format := m.Format
	switch format {
	case "", "auto":
		format = "auto"
	case "table", "time_series":
	default:
		return prepared{}, fmt.Errorf("no format %q: auto, table or time_series", m.Format)
	}
	return prepared{refID: q.RefID, format: format, bound: b}, nil
}

// QueryData answers a panel's queries, or an alert rule's. One query is one
// POST /query, sent with the ETag of the answer it had last time; several
// are one read-only POST /batch, which the server answers as one snapshot,
// every statement at the same change.
func (d *Datasource) QueryData(ctx context.Context, req *backend.QueryDataRequest) (*backend.QueryDataResponse, error) {
	resp := backend.NewQueryDataResponse()
	if d.err != nil {
		for _, q := range req.Queries {
			resp.Responses[q.RefID] = backend.ErrDataResponseWithSource(backend.StatusBadRequest, backend.ErrorSourceDownstream, d.err.Error())
		}
		return resp, nil
	}
	var ready []prepared
	var want []string
	for _, q := range req.Queries {
		p, err := prepare(q)
		if err != nil {
			resp.Responses[q.RefID] = backend.ErrDataResponseWithSource(backend.StatusBadRequest, backend.ErrorSourceDownstream, err.Error())
			continue
		}
		ready = append(ready, p)
		want = append(want, p.bound.Shape.Collection)
		want = append(want, p.bound.Shape.Lookups...)
	}
	if len(ready) == 0 {
		return resp, nil
	}
	schemas := d.schemas.get(ctx, d.client, want)
	waits := false
	for _, p := range ready {
		waits = waits || p.bound.After > 0
	}
	if len(ready) == 1 || waits {
		for _, p := range ready {
			resp.Responses[p.refID] = d.single(ctx, p, schemas)
		}
		return resp, nil
	}
	d.batch(ctx, ready, schemas, resp)
	return resp, nil
}

// single sends one statement, with the tag of its last answer: a 304 is
// answered from the kept body.
func (d *Datasource) single(ctx context.Context, p prepared, schemas Schemas) backend.DataResponse {
	key := cacheKey(p.bound.Text, p.bound.Params)
	c := d.client
	prev, had := d.answers.get(key)
	if had {
		c = c.IfNoneMatch(prev.etag)
	}
	if p.bound.After > 0 {
		c = c.After(p.bound.After)
		// Fenec-Wait beside Fenec-After bounds the wait for the change; on
		// a read alone it holds nothing, but it is sent only where it says
		// something.
		if d.settings.Wait > 0 {
			c = c.Wait(d.settings.Wait)
		}
	}
	r, err := c.QueryRaw(ctx, p.bound.Text, p.bound.Params...)
	if err != nil {
		return errorResponse(err)
	}
	body := r.JSON
	if r.NotModified {
		if !had {
			return backend.ErrDataResponseWithSource(backend.StatusBadGateway, backend.ErrorSourceDownstream,
				"fenec-server answered 304 to a statement the data source holds no answer to")
		}
		body = prev.body
	} else {
		d.answers.put(key, r.ETag, body)
	}
	a, err := parseAnswer(body)
	if err != nil {
		return backend.ErrDataResponseWithSource(backend.StatusBadRequest, backend.ErrorSourceDownstream, err.Error())
	}
	return framed(a, schemas, p, map[string]any{"etag": r.ETag, "notModified": r.NotModified})
}

// batch sends several statements as one read-only /batch. A statement that
// stops it (`at`) is answered with its refusal and the rest each on its
// own, so one wrong query does not blank the panel's others.
func (d *Datasource) batch(ctx context.Context, ready []prepared, schemas Schemas, resp *backend.QueryDataResponse) {
	stmts := make([]fenecdb.Statement, len(ready))
	for i, p := range ready {
		stmts[i] = fenecdb.Stmt(p.bound.Text, p.bound.Params...)
	}
	r, err := d.client.BatchRaw(ctx, stmts...)
	if err == nil {
		var all []answer
		if all, err = parseBatch(r.JSON); err == nil && len(all) == len(ready) {
			for i, p := range ready {
				resp.Responses[p.refID] = framed(all[i], schemas, p, map[string]any{"seq": r.Seq, "batch": true})
			}
			return
		}
		if err == nil {
			err = fmt.Errorf("a batch of %d statements answered %d", len(ready), len(all))
		}
	}
	var fe *fenecdb.Error
	stopped := -1
	if errors.As(err, &fe) && fe.At >= 0 && fe.At < len(ready) {
		stopped = fe.At
		resp.Responses[ready[stopped].refID] = errorResponse(err)
	}
	for i, p := range ready {
		if i != stopped {
			resp.Responses[p.refID] = d.single(ctx, p, schemas)
		}
	}
}

func framed(a answer, schemas Schemas, p prepared, custom map[string]any) backend.DataResponse {
	frames, err := Frames(a, schemas, FrameOptions{RefID: p.refID, Format: p.format, Shape: p.bound.Shape, Executed: executed(p.bound)})
	if err != nil {
		return backend.ErrDataResponseWithSource(backend.StatusBadRequest, backend.ErrorSourceDownstream, err.Error())
	}
	if len(frames) > 0 && frames[0].Meta != nil {
		frames[0].Meta.Custom = map[string]any{"fenecdb": custom}
	}
	return backend.DataResponse{Frames: frames}
}

// executed is the statement as sent and its parameters, for the query
// inspector.
func executed(b Bound) string {
	if len(b.Params) == 0 {
		return b.Text
	}
	var out strings.Builder
	out.WriteString(b.Text)
	for i, p := range b.Params {
		v, _ := json.Marshal(p)
		fmt.Fprintf(&out, "\n-- $%d = %s", i+1, v)
	}
	return out.String()
}

// errorResponse says what the server refused, with its status: a parse
// error's position, a policy's 403, an unmet `require`'s 412.
func errorResponse(err error) backend.DataResponse {
	var fe *fenecdb.Error
	if errors.As(err, &fe) {
		msg := fmt.Sprintf("fenec-server answered %d: %s", fe.Status, fe.Message)
		switch fe.Status {
		case 401:
			msg = "fenec-server refused the data source's token (401): " + fe.Message
		case 403:
			msg = "outside what the data source's token may read (403): " + fe.Message
		case 404:
			msg = "not on the server (404): " + fe.Message
		}
		return backend.ErrDataResponseWithSource(backend.Status(fe.Status), backend.ErrorSourceDownstream, msg)
	}
	// A dashboard left, or a refresh asked again, before the answer came:
	// Grafana cancels the request, and the server is not the one that
	// failed.
	if errors.Is(err, context.Canceled) {
		return backend.ErrDataResponseWithSource(backend.StatusBadRequest, backend.ErrorSourceDownstream, "the query was canceled before fenec-server answered")
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return backend.ErrDataResponseWithSource(backend.StatusTimeout, backend.ErrorSourceDownstream, "fenec-server did not answer in time: "+err.Error())
	}
	return backend.ErrDataResponseWithSource(backend.StatusBadGateway, backend.ErrorSourceDownstream, "fenec-server could not be reached: "+err.Error())
}

// CheckHealth is the data source's "Save & test": the server answers
// /_health, and the token reads -- the collections it can see, then a
// `get` of the first.
func (d *Datasource) CheckHealth(ctx context.Context, _ *backend.CheckHealthRequest) (*backend.CheckHealthResult, error) {
	fail := func(msg string) (*backend.CheckHealthResult, error) {
		return &backend.CheckHealthResult{Status: backend.HealthStatusError, Message: msg}, nil
	}
	if d.err != nil {
		return fail(d.err.Error())
	}
	if err := d.client.Health(ctx); err != nil {
		return fail(fmt.Sprintf("fenec-server at %s did not answer /_health: %v", d.settings.URL, err))
	}
	names, err := listCollections(ctx, d.client)
	if err != nil {
		return fail(errorResponse(err).Error.Error())
	}
	var readable []string
	for _, n := range names {
		if !strings.HasPrefix(n, "_") {
			readable = append(readable, n)
		}
	}
	if len(readable) == 0 {
		return fail("fenec-server answers, but the token reads no collection here: give it a policy's role that reads one")
	}
	// A name the server gave, and of a name's characters: no text a user
	// typed goes into this statement.
	first := readable[0]
	if validName(first) {
		if _, err := d.client.Query(ctx, "get "+first+" select id limit 1"); err != nil {
			return fail(fmt.Sprintf("the token lists %s but cannot read it: %s", first, errorResponse(err).Error))
		}
	}
	shown := readable
	if len(shown) > 5 {
		shown = append(append([]string(nil), shown[:5]...), "...")
	}
	return &backend.CheckHealthResult{
		Status:  backend.HealthStatusOk,
		Message: fmt.Sprintf("fenec-server answers, and the token reads %d collection(s): %s", len(readable), strings.Join(shown, ", ")),
	}, nil
}

// listCollections is the `collections` statement's names, which a scoped
// token answers with the collections its policy names.
func listCollections(ctx context.Context, c *fenecdb.Client) ([]string, error) {
	s, err := describe(ctx, c)
	if err != nil {
		return nil, err
	}
	names := make([]string, 0, len(s))
	for n := range s {
		names = append(names, n)
	}
	sort.Strings(names)
	return names, nil
}

func describe(ctx context.Context, c *fenecdb.Client) (Schemas, error) {
	type field struct {
		Name string `json:"name"`
		Type string `json:"type"`
	}
	type collection struct {
		Name   string  `json:"name"`
		Fields []field `json:"fields"`
	}
	list, err := fenecdb.QueryAs[collection](ctx, c, "collections")
	if err != nil {
		return nil, err
	}
	out := make(Schemas, len(list))
	for _, col := range list {
		fields := make(map[string]string, len(col.Fields))
		for _, f := range col.Fields {
			fields[f.Name] = f.Type
		}
		out[col.Name] = fields
	}
	return out, nil
}

// schemaCache holds the collections' schemas for a while: they type a
// column, which an alter changes rarely, and asking for them with every
// query would double its requests.
type schemaCache struct {
	mu sync.Mutex
	at time.Time
	s  Schemas
}

const (
	schemaFresh = 30 * time.Second
	// A collection the last listing lacked -- made since -- is asked for
	// again, but not more often than this.
	schemaRetry = 5 * time.Second
)

func (c *schemaCache) get(ctx context.Context, client *fenecdb.Client, want []string) Schemas {
	c.mu.Lock()
	defer c.mu.Unlock()
	age := time.Since(c.at)
	missing := false
	for _, w := range want {
		if _, ok := c.s[w]; w != "" && !ok {
			missing = true
		}
	}
	if c.s != nil && age < schemaFresh && !(missing && age > schemaRetry) {
		return c.s
	}
	s, err := describe(ctx, client)
	if err != nil {
		// Typed by values alone until the next try: a column's type is a
		// refinement, never a reason to fail a query.
		log.DefaultLogger.Debug("collections", "error", err)
		c.at = time.Now()
		return c.s
	}
	c.s, c.at = s, time.Now()
	return s
}

// explainRequest is what the editor's Explain sends: the query, and the
// range and interval its builtins take.
type explainRequest struct {
	Query         Query `json:"query"`
	From          int64 `json:"from"`
	To            int64 `json:"to"`
	IntervalMs    int64 `json:"intervalMs"`
	MaxDataPoints int64 `json:"maxDataPoints"`
}

// CallResource answers the query editor: POST explain is the statement as
// the plugin binds it and the server's plan for it.
func (d *Datasource) CallResource(ctx context.Context, req *backend.CallResourceRequest, sender backend.CallResourceResponseSender) error {
	reply := func(status int, v any) error {
		body, _ := json.Marshal(v)
		return sender.Send(&backend.CallResourceResponse{
			Status:  status,
			Headers: map[string][]string{"Content-Type": {"application/json"}},
			Body:    body,
		})
	}
	if req.Path != "explain" {
		return reply(http.StatusNotFound, map[string]string{"error": "no resource " + req.Path})
	}
	if req.Method != http.MethodPost {
		return reply(http.StatusMethodNotAllowed, map[string]string{"error": "explain is a POST"})
	}
	if d.err != nil {
		return reply(http.StatusBadRequest, map[string]string{"error": d.err.Error()})
	}
	var er explainRequest
	if err := json.Unmarshal(req.Body, &er); err != nil {
		return reply(http.StatusBadRequest, map[string]string{"error": "the request's JSON: " + err.Error()})
	}
	b, err := Bind(er.Query, Builtins{
		From:          time.UnixMilli(er.From),
		To:            time.UnixMilli(er.To),
		Interval:      time.Duration(er.IntervalMs) * time.Millisecond,
		MaxDataPoints: er.MaxDataPoints,
	})
	if err != nil {
		return reply(http.StatusBadRequest, map[string]string{"error": err.Error()})
	}
	text := b.Text
	if b.Shape.Verb != "explain" {
		// The keyword before the statement as written: `explain` takes a
		// `get` or a `select`, which Bind held the text to.
		text = "explain " + text
	}
	rows, err := d.client.Query(ctx, text, b.Params...)
	if err != nil {
		return reply(http.StatusBadRequest, map[string]string{"error": errorResponse(err).Error.Error()})
	}
	plan := make([]string, 0, len(rows))
	for _, r := range rows {
		if s, ok := r["plan"].(string); ok {
			plan = append(plan, s)
		}
	}
	return reply(http.StatusOK, map[string]any{"statement": executed(b), "plan": plan})
}
