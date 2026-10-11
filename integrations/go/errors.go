package fenecdb

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"
)

// The codes an *Error carries, named after its status: the server answers
// a refusal with a status and {"error": message}, and these are the
// statuses it uses.
const (
	CodeBadRequest   = "bad_request"  // 400: a statement that does not parse, a bad parameter
	CodeUnauthorized = "unauthorized" // 401: no token, or a wrong one
	CodeForbidden    = "forbidden"    // 403: outside a token's policy, a write to a replica
	CodeNotFound     = "not_found"    // 404: a collection, field or tenant that is not there
	CodeConflict     = "conflict"     // 409: an id or a @unique value taken, a collection that exists
	CodeGone         = "gone"         // 410: a /_changes cursor the server no longer reaches
	CodeUnmet        = "unmet"        // 412: a write's Require not met, its batch put back
	CodeKeyReused    = "key_reused"   // 422: an Idempotency-Key sent with another request
	CodeTooMany      = "too_many"     // 429: a tenant's quota of streams, requests or writes a second
	CodeUnavailable  = "unavailable"  // 503: too many connections or subscriptions, a tenant moved or failed over
	CodeTimeout      = "timeout"      // 504: After's change did not reach the replica in the wait
	CodeFull         = "storage_full" // 507: past --max-memory, or a tenant's quota of bytes or rows
)

// The quotas a tenant node holds a tenant to, as an *Error's Quota names
// the one that refused it: QuotaBytes and QuotaRows answer 507, and room is
// made by deleting; the rest answer 429 with a RetryAfter.
const (
	QuotaBytes      = "bytes"
	QuotaRows       = "rows"
	QuotaStreams    = "streams"
	QuotaRequests   = "requests"
	QuotaWrites     = "writes"
	QuotaWriteBytes = "write_bytes"
)

// Error is a request the server refused. errors.As finds it:
//
//	var e *fenecdb.Error
//	if errors.As(err, &e) && e.Status == 404 { ... }
type Error struct {
	Status  int    // the HTTP status
	Code    string // the status's name, one of the Code constants or http_<status>
	Message string // the server's {"error": ...}
	// Completed is how many statements of a failed Batch were applied: 0,
	// since a batch lands whole or not at all, but for one holding a
	// compact, which runs each statement on its own and keeps what ran.
	Completed int
	// At is the statement of a failed Batch that stopped it, from 0: the
	// write whose Require was not met, the put whose id was taken. -1 for
	// anything but a batch's stop, since 0 names its first statement.
	At int
	// Quota is the tenant's quota that refused the request, one of the
	// Quota constants; "" for any other refusal.
	Quota string
	// RetryAfter is how long to wait before sending again, where the
	// server said -- a quota's 429, the 503 of a tenant being moved or
	// failed over or of a server at its ceiling: 0 otherwise.
	RetryAfter time.Duration
}

func (e *Error) Error() string { return fmt.Sprintf("fenecdb: %d %s", e.Status, e.Message) }

func codeOf(status int) string {
	switch status {
	case 400:
		return CodeBadRequest
	case 401:
		return CodeUnauthorized
	case 403:
		return CodeForbidden
	case 404:
		return CodeNotFound
	case 409:
		return CodeConflict
	case 410:
		return CodeGone
	case 412:
		return CodeUnmet
	case 422:
		return CodeKeyReused
	case 429:
		return CodeTooMany
	case 503:
		return CodeUnavailable
	case 504:
		return CodeTimeout
	case 507:
		return CodeFull
	}
	return fmt.Sprintf("http_%d", status)
}

func errorOf(status int, raw []byte) *Error {
	e := &Error{Status: status, Code: codeOf(status), At: -1}
	var body struct {
		Error      string `json:"error"`
		Completed  int    `json:"completed"`
		At         *int   `json:"at"`
		Quota      string `json:"quota"`
		RetryAfter int64  `json:"retry_after"`
	}
	if json.Unmarshal(raw, &body) == nil && body.Error != "" {
		e.Message, e.Completed, e.Quota = body.Error, body.Completed, body.Quota
		// The wait Retry-After says, in the body as well: what reads it
		// here has no headers.
		e.RetryAfter = time.Duration(body.RetryAfter) * time.Second
		if body.At != nil {
			e.At = *body.At
		}
	} else {
		e.Message = strings.TrimSpace(string(raw))
	}
	return e
}
