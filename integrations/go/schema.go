package fenecdb

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
)

// SchemaPlan is what the engine found comparing the server's schema with
// the one the code declares, and did: the FenecQL that adds what is
// missing, the migrations it recorded, and what it refused, each with how
// to resolve it.
type SchemaPlan struct {
	Applied    bool            `json:"applied"`
	Ran        bool            `json:"ran"`
	Migrations []int           `json:"migrations"`
	Statements []string        `json:"statements"`
	Refusals   []SchemaRefusal `json:"refusals"`
}

// SchemaRefusal is a difference no open applies: one that would lose data
// or could mean two things.
type SchemaRefusal struct {
	Kind       string  `json:"kind"`
	Collection string  `json:"collection"`
	Field      *string `json:"field"`
	Message    string  `json:"message"`
	Fix        string  `json:"fix"`
}

// SchemaError is a schema the server's differs from in what no open
// applies; errors.As finds it, and Refusals says each difference.
type SchemaError struct {
	Refusals []SchemaRefusal
}

func (e *SchemaError) Error() string {
	var b strings.Builder
	b.WriteString("fenecdb: the database's schema differs from the code's:")
	for _, r := range e.Refusals {
		b.WriteString("\n  - " + r.Message + "\n    " + r.Fix)
	}
	return b.String()
}

// Rebuild is a migration making a field again as the code declares it --
// its index, its collation -- its values copied: what changes an index,
// which no statement changes in place.
func Rebuild(collection, field string) any {
	return map[string]any{"rebuild": map[string]string{"collection": collection, "field": field}}
}

// Schema checks the server's schema against fenecql -- `create collection`
// and `create index` statements, a schema.fenecql file -- in the engine.
// The server owns its schema: it is compared, and nothing applied, unless
// migrate -- with the server's token -- runs the migrations it has not
// recorded, in order, and adds what only adds, all one block. A migration
// is FenecQL text or Rebuild(...). A difference that would lose data or
// could mean two things is a *SchemaError.
func (c *Client) Schema(ctx context.Context, fenecql string, migrations []any, migrate bool) (SchemaPlan, error) {
	if migrations == nil {
		migrations = []any{}
	}
	body, err := json.Marshal(map[string]any{"format": 1, "fenecql": fenecql, "migrations": migrations})
	if err != nil {
		return SchemaPlan{}, err
	}
	path := "/_schema/plan?mode=follow"
	if migrate {
		path = "/_schema/apply"
	}
	var plan SchemaPlan
	raw, _, err := c.do(ctx, "POST", path, "application/json", body, 0)
	var refused *Error
	switch {
	case errors.As(err, &refused) && refused.Status == 409:
		// Refusals, which the answer holds as a plan does.
		raw = []byte(refused.Message)
	case err != nil:
		return plan, err
	}
	if err := json.Unmarshal(raw, &plan); err != nil {
		return plan, err
	}
	if len(plan.Refusals) > 0 {
		return plan, &SchemaError{Refusals: plan.Refusals}
	}
	return plan, nil
}
