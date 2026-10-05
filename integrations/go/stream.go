package fenecdb

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// Event is one event of a subscription.
type Event struct {
	// Type is "seed" -- Rows is the whole shape, which replaces what the
	// subscriber held -- "change" -- Puts and Dels since the last event --
	// or "error", the last event, with Err: an *Error of Status 401 where
	// the server ended the stream at its token's exp.
	Type   string
	Seq    uint64 // the change the event brings the shape up to
	Rows   []Row
	Puts   []Row
	Dels   []int64
	Schema bool // the collection's schema changed
	Err    error
}

// Subscribe streams a collection's rows that match filter, and every change
// to them, over server-sent events (GET /<collection>/changes): a seed
// first, then each change as it lands. filter is the REST surface's query
// string -- year=gte.2024, where=<a FenecQL condition>, select=title,year,
// since=<a seq> to resume -- and takes no parameters. The channel closes
// when ctx is done or the stream ends; a stream that ends for any other
// reason sends an "error" event first.
func (c *Client) Subscribe(ctx context.Context, collection string, filter url.Values) (<-chan Event, error) {
	path := c.base + "/" + url.PathEscape(collection) + "/changes"
	if q := filter.Encode(); q != "" {
		path += "?" + q
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, path, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "text/event-stream")
	c.headers(req)
	res, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	if res.StatusCode != http.StatusOK {
		defer res.Body.Close()
		raw, _ := io.ReadAll(res.Body)
		return nil, errorOf(res.StatusCode, raw)
	}
	out := make(chan Event, 16)
	go func() {
		defer close(out)
		defer res.Body.Close()
		send := func(e Event) bool {
			select {
			case out <- e:
				return true
			case <-ctx.Done():
				return false
			}
		}
		r := bufio.NewReader(res.Body)
		var name string
		var data []byte
		for {
			line, err := r.ReadBytes('\n')
			if err != nil {
				if ctx.Err() == nil {
					if errors.Is(err, io.EOF) {
						err = errors.New("fenecdb: the subscription ended")
					}
					send(Event{Type: "error", Err: err})
				}
				return
			}
			line = bytes.TrimRight(line, "\r\n")
			switch {
			case len(line) == 0:
				if name != "" || data != nil {
					e := eventOf(name, data)
					if !send(e) || e.Type == "error" {
						return
					}
				}
				name, data = "", nil
			case line[0] == ':': // a keep-alive
			case bytes.HasPrefix(line, []byte("event:")):
				name = strings.TrimSpace(string(line[6:]))
			case bytes.HasPrefix(line, []byte("data:")):
				data = append(data, bytes.TrimPrefix(line[5:], []byte(" "))...)
			}
		}
	}()
	return out, nil
}

func eventOf(name string, data []byte) Event {
	var body struct {
		Seq    uint64  `json:"seq"`
		Rows   []Row   `json:"rows"`
		Puts   []Row   `json:"puts"`
		Dels   []int64 `json:"dels"`
		Schema bool    `json:"schema"`
		Error  string  `json:"error"`
		Status int     `json:"status"`
	}
	if err := json.Unmarshal(data, &body); err != nil {
		return Event{Type: "error", Err: fmt.Errorf("fenecdb: an event of the subscription: %w", err)}
	}
	if name == "error" {
		// The server ends a stream at its token's exp with status 401: an
		// *Error, as the refused request it stands for, for the caller to
		// subscribe again with a fresh token.
		if body.Status != 0 {
			return Event{Type: "error", Err: errorOf(body.Status, data)}
		}
		return Event{Type: "error", Err: errors.New("fenecdb: " + body.Error)}
	}
	return Event{Type: name, Seq: body.Seq, Rows: body.Rows, Puts: body.Puts, Dels: body.Dels, Schema: body.Schema}
}

// Change is one write on the server's disk.
type Change struct {
	Seq        uint64 `json:"seq"`
	At         int64  `json:"at"` // when the primary wrote it, ms since the epoch
	Collection string `json:"collection"`
	Op         string `json:"op"` // put, del, create, alter or drop
	ID         int64  `json:"id"`
	Doc        Row    `json:"doc"` // a put's document, as written
}

// ChangeBatch is what Changes hands over, and Next the last write it holds:
// the since to read on from, so that nothing is missed or had twice.
type ChangeBatch struct {
	Changes []Change
	Next    uint64
}

// Changes reads the writes on the server's disk after since (GET
// /_changes; fenec-server --cdc, or a primary with --replication-token):
// 1 000 at most. With wait, an answer that would hold none waits that long
// for a write, 30 seconds at most. A since the server no longer keeps is a
// 410, whose Message says the first it does.
func (c *Client) Changes(ctx context.Context, since uint64, wait time.Duration) (ChangeBatch, error) {
	q := url.Values{"since": {strconv.FormatUint(since, 10)}}
	if wait > 0 {
		q.Set("wait", strconv.FormatInt(wait.Milliseconds(), 10))
	}
	raw, h, err := c.do(ctx, http.MethodGet, "/_changes?"+q.Encode(), "", nil, wait)
	if err != nil {
		return ChangeBatch{}, err
	}
	b := ChangeBatch{Next: since}
	if h.hasNext {
		b.Next = h.next
	}
	for _, line := range bytes.Split(raw, []byte("\n")) {
		if len(bytes.TrimSpace(line)) == 0 {
			continue
		}
		var ch Change
		if err := json.Unmarshal(line, &ch); err != nil {
			return ChangeBatch{}, fmt.Errorf("fenecdb: a line of /_changes: %w", err)
		}
		b.Changes = append(b.Changes, ch)
	}
	return b, nil
}
