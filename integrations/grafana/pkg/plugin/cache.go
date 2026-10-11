package plugin

import (
	"container/list"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"sync"
)

// answers keeps the last answer to each statement and its parameters with
// the ETag it came with, so the same request asked again goes out with
// If-None-Match and a 304 is answered from here. The tag fenec-server gives
// names a change, not the statement: an answer is only ever matched to the
// request it answered (cacheKey), never to another one's tag.
//
// Grafana's own query caching would do this for any data source, but it is
// Grafana Enterprise's and Cloud's, not the open-source build's.
type answers struct {
	mu      sync.Mutex
	limit   int // bytes held at most
	size    int
	byKey   map[string]*list.Element
	recency *list.List // front: the latest used
}

type kept struct {
	key  string
	etag string
	body []byte
}

// An answer past this is not kept: a refresh of one that big is a query to
// write with `group` and `limit`, not to keep.
const keepAtMost = 4 << 20

func newAnswers(limit int) *answers {
	return &answers{limit: limit, byKey: map[string]*list.Element{}, recency: list.New()}
}

// cacheKey is a statement and its parameters, as the server is sent them.
func cacheKey(text string, params []any) string {
	h := sha256.New()
	h.Write([]byte(text))
	h.Write([]byte{0})
	p, _ := json.Marshal(params)
	h.Write(p)
	return hex.EncodeToString(h.Sum(nil))
}

func (c *answers) get(key string) (kept, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	e, ok := c.byKey[key]
	if !ok {
		return kept{}, false
	}
	c.recency.MoveToFront(e)
	return e.Value.(kept), true
}

func (c *answers) put(key, etag string, body []byte) {
	if etag == "" || len(body) > keepAtMost {
		c.drop(key)
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if e, ok := c.byKey[key]; ok {
		c.size -= len(e.Value.(kept).body)
		c.recency.Remove(e)
		delete(c.byKey, key)
	}
	c.byKey[key] = c.recency.PushFront(kept{key, etag, body})
	c.size += len(body)
	for c.size > c.limit {
		last := c.recency.Back()
		k := last.Value.(kept)
		c.size -= len(k.body)
		c.recency.Remove(last)
		delete(c.byKey, k.key)
	}
}

func (c *answers) drop(key string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if e, ok := c.byKey[key]; ok {
		c.size -= len(e.Value.(kept).body)
		c.recency.Remove(e)
		delete(c.byKey, key)
	}
}
