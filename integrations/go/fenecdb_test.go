package fenecdb_test

// Against real fenec-server processes, started here from the binary
// `cargo build -p fenec-server` makes (FENEC_SERVER names another): a
// primary with a token, a policy for JSON Web Tokens and the change stream,
// and as a test needs them a replica of it and a node of tenants.

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"math"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	fenecdb "github.com/fenecdb/fenec/integrations/go"
)

const (
	rootToken = "go-tests"
	replToken = "go-repl"
	jwtSecret = "go-tests-jwt-secret-of-32-bytes-or-more"
)

var (
	binary  string
	scratch string
	primary string // the primary's URL
)

func TestMain(m *testing.M) {
	binary = os.Getenv("FENEC_SERVER")
	if binary == "" {
		binary, _ = filepath.Abs("../../target/debug/fenec-server")
	}
	if _, err := os.Stat(binary); err != nil {
		fmt.Fprintf(os.Stderr, "no fenec-server at %s: cargo build -p fenec-server\n", binary)
		os.Exit(1)
	}
	var err error
	if scratch, err = os.MkdirTemp("", "fenecdb-go"); err != nil {
		panic(err)
	}
	policy := filepath.Join(scratch, "policy.txt")
	if err := os.WriteFile(policy, []byte("notes read,write where owner = $jwt.sub\n"), 0o600); err != nil {
		panic(err)
	}
	var stop func()
	primary, stop = serve("--file", filepath.Join(scratch, "primary.fenec"),
		"--http-token", rootToken, "--replication-token", replToken,
		"--jwt-secret", jwtSecret, "--policy", policy, "--sync", "50")
	code := m.Run()
	stop()
	for _, s := range started {
		s()
	}
	os.RemoveAll(scratch)
	os.Exit(code)
}

var started []func()

// serve starts a fenec-server on a port of its own and waits for it to say
// where it listens.
func serve(args ...string) (string, func()) {
	cmd := exec.Command(binary, append([]string{"--http", "127.0.0.1:0"}, args...)...)
	stderr, err := cmd.StderrPipe()
	if err != nil {
		panic(err)
	}
	if err := cmd.Start(); err != nil {
		panic(err)
	}
	lines := bufio.NewScanner(stderr)
	var addr string
	var log strings.Builder
	for addr == "" && lines.Scan() {
		line := lines.Text()
		log.WriteString(line + "\n")
		if _, rest, ok := strings.Cut(line, "listening on: http://"); ok {
			addr = strings.Fields(rest)[0]
		}
	}
	if addr == "" {
		panic("fenec-server did not start:\n" + log.String())
	}
	go func() {
		for lines.Scan() {
		}
	}()
	stop := func() {
		cmd.Process.Kill()
		cmd.Wait()
	}
	return "http://" + addr, stop
}

var names atomic.Int64

// fresh is a collection name no other test uses.
func fresh(prefix string) string {
	return fmt.Sprintf("%s_%d_%d", prefix, time.Now().UnixNano()%1e6, names.Add(1))
}

func root() *fenecdb.Client { return fenecdb.New(primary, fenecdb.WithToken(rootToken)) }

// must(db.Exec(...)).of(t): the value, or the test stops at the error.
func must[T any](v T, err error) result[T] { return result[T]{v, err} }

type result[T any] struct {
	v   T
	err error
}

func (r result[T]) of(t *testing.T) T {
	t.Helper()
	if r.err != nil {
		t.Fatal(r.err)
	}
	return r.v
}

func statusOf(t *testing.T, err error) *fenecdb.Error {
	t.Helper()
	var e *fenecdb.Error
	if !errors.As(err, &e) {
		t.Fatalf("not a *fenecdb.Error: %v", err)
	}
	return e
}

func TestCreatePutGetAndNearWithExactScores(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("docs")
	must(db.Exec(ctx, "create collection "+name+" (title text, embed vector<3> @hnsw(dot))")).of(t)
	r := must(db.Exec(ctx, "put "+name+" {title: $1, embed: $2}", "first", []float32{1, 2, 3})).of(t)
	if r.Affected != 1 || r.Seq == 0 || db.Seq() < r.Seq {
		t.Fatalf("a put answered %+v, the client holds seq %d", r, db.Seq())
	}
	must(db.Exec(ctx, "put "+name+" {title: $1, embed: $2}", "second", []float32{0.5, 0.25, 0})).of(t)

	rows := must(db.Query(ctx, "get "+name+" where id = $1", 1)).of(t)
	if len(rows) != 1 || rows[0]["title"] != "first" || !reflect.DeepEqual(rows[0]["embed"], []any{1.0, 2.0, 3.0}) {
		t.Fatalf("get by id answered %v", rows)
	}

	type hit struct {
		Title string  `json:"title"`
		Score float32 `json:"_score"`
	}
	hits := must(fenecdb.QueryAs[hit](ctx, db, "get "+name+" select title near embed $1 limit 5", []float32{1, 0, 0})).of(t)
	if want := []hit{{"first", 1}, {"second", 0.5}}; !reflect.DeepEqual(hits, want) {
		t.Fatalf("near answered %v, want %v", hits, want)
	}
	// A write's answer through Query is one row holding it.
	if rows := must(db.Query(ctx, "del "+name+" where id = 2")).of(t); rows[0]["affected"] != 1.0 {
		t.Fatalf("del through Query answered %v", rows)
	}
}

// 7.038531e-26: the f32 whose shortest text an f64 reader rounds away.
var tie = math.Float32frombits(0x15ae43fd)

func TestParamsOfEveryType(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("params")
	must(db.Exec(ctx, "create collection "+name+
		" (t text, n int, f float, b bool, gone text, tags [text], embed vector<5>, meta json)")).of(t)
	vec := []float32{tie, 0.1, math.MaxFloat32, -1.5e-7, math.SmallestNonzeroFloat32}
	must(db.Exec(ctx, "put "+name+" {t: $1, n: $2, f: $3, b: $4, gone: $5, tags: $6, embed: $7, meta: $8}",
		"çağ \"quoted\"", int64(-1<<53), 2.5, true, nil, []string{"a", "b"}, vec,
		map[string]any{"lang": "tr", "rank": 2})).of(t)

	type row struct {
		ID    int64          `json:"id"`
		T     string         `json:"t"`
		N     int64          `json:"n"`
		F     float64        `json:"f"`
		B     bool           `json:"b"`
		Gone  *string        `json:"gone"`
		Tags  []string       `json:"tags"`
		Embed []float32      `json:"embed"`
		Meta  map[string]any `json:"meta"`
	}
	got := must(fenecdb.QueryAs[row](ctx, db,
		"get "+name+" where t = $1 and n = $2 and f = $3 and b = $4 and gone is null and tags has $5 and n in [$6, $2]",
		"çağ \"quoted\"", int64(-1<<53), float32(2.5), true, "b", 7)).of(t)
	if len(got) != 1 {
		t.Fatalf("the filter of every type found %d rows", len(got))
	}
	g := got[0]
	if g.T != "çağ \"quoted\"" || g.N != -1<<53 || g.F != 2.5 || !g.B || g.Gone != nil ||
		!reflect.DeepEqual(g.Tags, []string{"a", "b"}) || g.Meta["lang"] != "tr" || g.Meta["rank"] != 2.0 {
		t.Fatalf("read back %+v", g)
	}
	for i := range vec {
		if math.Float32bits(g.Embed[i]) != math.Float32bits(vec[i]) {
			t.Fatalf("component %d: wrote %g (%08x), read %g (%08x)", i, vec[i],
				math.Float32bits(vec[i]), g.Embed[i], math.Float32bits(g.Embed[i]))
		}
	}
	if _, err := db.Query(ctx, "get "+name+" where f = $1", float32(math.NaN())); err == nil {
		t.Fatal("a NaN was sent")
	}
}

func TestRefusalsAreTypedErrors(t *testing.T) {
	ctx := context.Background()
	db := root()
	_, err := db.Query(ctx, "get no_such_collection_here")
	if e := statusOf(t, err); e.Status != 404 || e.Code != fenecdb.CodeNotFound || !strings.Contains(e.Message, "no_such_collection_here") {
		t.Fatalf("a missing collection: %+v", e)
	}
	_, err = db.Query(ctx, "get x wher")
	if e := statusOf(t, err); e.Status != 400 || e.Code != fenecdb.CodeBadRequest {
		t.Fatalf("a statement that does not parse: %+v", e)
	}
	name := fresh("dup")
	must(db.Exec(ctx, "create collection "+name+" (t text)")).of(t)
	must(db.Exec(ctx, "insert "+name+" {id: 1, t: $1}", "a")).of(t)
	_, err = db.Exec(ctx, "insert "+name+" {id: 1, t: $1}", "b")
	if e := statusOf(t, err); e.Status != 409 || e.Code != fenecdb.CodeConflict {
		t.Fatalf("an id taken: %+v", e)
	}
	_, err = fenecdb.New(primary, fenecdb.WithToken("wrong")).Query(ctx, "collections")
	if e := statusOf(t, err); e.Status != 401 {
		t.Fatalf("a wrong token: %+v", e)
	}
}

func TestABatchLandsWholeOrNotAtAll(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("batch")
	must(db.Exec(ctx, "create collection "+name+" (t text, n int)")).of(t)
	out := must(db.Batch(ctx,
		fenecdb.Stmt("put "+name+" {t: $1, n: $2}", "a", 1),
		fenecdb.Stmt("put "+name+" {t: $1, n: $2}", "b", 2),
		fenecdb.Stmt("get "+name+" select t where n >= $1 order n", 1))).of(t)
	if out.OK != 3 || out.Results[0].Affected != 1 || len(out.Results[2].Rows) != 2 || out.Seq == 0 {
		t.Fatalf("a batch answered %+v", out)
	}
	_, err := db.Batch(ctx,
		fenecdb.Stmt("put "+name+" {t: $1, n: $2}", "c", 3),
		fenecdb.Stmt("del "+name+" where n = 1"),
		fenecdb.Stmt("put "+name+" {nofield: 1}"))
	e := statusOf(t, err)
	if e.Status != 404 || e.Completed != 0 || e.At != 2 {
		t.Fatalf("a failing batch: %+v", e)
	}
	rows := must(db.Query(ctx, "get "+name+" select t order n")).of(t)
	if len(rows) != 2 || rows[0]["t"] != "a" || rows[1]["t"] != "b" {
		t.Fatalf("a failed batch left %v", rows)
	}
}

func TestAWriteThatMissesItsCountIsRefusedAndPutBack(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("require")
	must(db.Exec(ctx, "create collection "+name+" (name text, balance int)")).of(t)
	accounts := db.From(name)
	must(accounts.Insert(ctx, fenecdb.D("name", "a", "balance", 10))).of(t)
	_, err := accounts.Where("name", "=", "nobody").Update(ctx, fenecdb.D("balance", 0), fenecdb.Require(1))
	if e := statusOf(t, err); e.Status != 412 || e.Code != fenecdb.CodeUnmet || !strings.Contains(e.Message, "requires 1") || e.At != -1 {
		t.Fatalf("an unmet update: %+v", e)
	}
	if r := must(accounts.Where("name", "=", "a").Update(ctx, fenecdb.D("balance", 5), fenecdb.Require(1))).of(t); r.Affected != 1 {
		t.Fatalf("a met update wrote %d", r.Affected)
	}
	// A batch whose second write is unmet keeps nothing of the first.
	met, metParams, err := accounts.Where("name", "=", "a").ToUpdate(fenecdb.D("balance", 0), fenecdb.Require(1))
	if err != nil {
		t.Fatal(err)
	}
	unmet, unmetParams, err := accounts.Where("name", "=", "nobody").ToDelete(fenecdb.Require(1))
	if err != nil {
		t.Fatal(err)
	}
	_, err = db.Batch(ctx, fenecdb.Stmt(met, metParams...), fenecdb.Stmt(unmet, unmetParams...))
	if e := statusOf(t, err); e.Status != 412 || e.Code != fenecdb.CodeUnmet || e.Completed != 0 || e.At != 1 {
		t.Fatalf("an unmet batch: %+v", e)
	}
	rows := must(db.Query(ctx, "get "+name+" select balance")).of(t)
	if len(rows) != 1 || rows[0]["balance"] != 5.0 {
		t.Fatalf("an unmet batch left %v", rows)
	}
}

func TestAnIdempotencyKeyIsReplayed(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("idem")
	must(db.Exec(ctx, "create collection "+name+" (t text)")).of(t)
	key := db.IdempotencyKey(name + "-1")
	first := must(key.Exec(ctx, "put "+name+" {t: $1}", "once")).of(t)
	again := must(key.Exec(ctx, "put "+name+" {t: $1}", "once")).of(t)
	if first.Replayed || !again.Replayed || again.Affected != 1 {
		t.Fatalf("first %+v, again %+v", first, again)
	}
	if rows := must(db.Query(ctx, "get "+name+" count")).of(t); rows[0]["count"] != 1.0 {
		t.Fatalf("a replayed write made %v", rows)
	}
	_, err := key.Exec(ctx, "put "+name+" {t: $1}", "another")
	if e := statusOf(t, err); e.Status != 422 || e.Code != fenecdb.CodeKeyReused {
		t.Fatalf("a key with another request: %+v", e)
	}
	// A batch under a key of its own lands once as well.
	batch := db.IdempotencyKey(name + "-2")
	stmts := []fenecdb.Statement{fenecdb.Stmt("put "+name+" {t: $1}", "b1"), fenecdb.Stmt("put "+name+" {t: $1}", "b2")}
	b1 := must(batch.Batch(ctx, stmts...)).of(t)
	b2 := must(batch.Batch(ctx, stmts...)).of(t)
	if b1.Replayed || !b2.Replayed || b2.OK != 2 || b1.Seq == 0 {
		t.Fatalf("first batch %+v, again %+v", b1, b2)
	}
	if rows := must(db.Query(ctx, "get "+name+" count")).of(t); rows[0]["count"] != 3.0 {
		t.Fatalf("a replayed batch made %v", rows)
	}
}

func TestAfterReadsYourWriteOnAReplica(t *testing.T) {
	ctx := context.Background()
	replica, stop := serve("--file", filepath.Join(scratch, "replica.fenec"),
		"--replication-token", replToken, "--replica-of", primary, "--sync", "50")
	defer stop()
	db := root()
	name := fresh("after")
	must(db.Exec(ctx, "create collection "+name+" (t text)")).of(t)
	w := must(db.Exec(ctx, "put "+name+" {t: $1}", "written")).of(t)
	r := fenecdb.New(replica)
	rows := must(r.After(w.Seq).Query(ctx, "get "+name+" select t")).of(t)
	if len(rows) != 1 || rows[0]["t"] != "written" {
		t.Fatalf("the replica answered %v after %d", rows, w.Seq)
	}
	_, err := r.Exec(ctx, "put "+name+" {t: $1}", "refused")
	if e := statusOf(t, err); e.Status != 403 {
		t.Fatalf("a write to a replica: %+v", e)
	}
}

func TestASubscriptionHearsAChange(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	db := root()
	name := fresh("sub")
	must(db.Exec(ctx, "create collection "+name+" (t text, n int)")).of(t)
	must(db.Exec(ctx, "put "+name+" {t: $1, n: $2}", "old", 10)).of(t)
	events := must(db.Subscribe(ctx, name, url.Values{"n": {"gte.10"}})).of(t)
	seed := <-events
	if seed.Type != "seed" || len(seed.Rows) != 1 || seed.Rows[0]["t"] != "old" {
		t.Fatalf("the seed: %+v", seed)
	}
	must(db.Exec(ctx, "put "+name+" {t: $1, n: $2}", "outside", 1)).of(t)
	must(db.Exec(ctx, "put "+name+" {t: $1, n: $2}", "new", 11)).of(t)
	// The row outside the shape comes as the deletion of an id the
	// subscriber never held: an unscoped shape tells of every id that
	// changed and does not match.
	change := <-events
	for change.Type == "change" && len(change.Puts) == 0 {
		change = <-events
	}
	if change.Type != "change" || len(change.Puts) != 1 || change.Puts[0]["t"] != "new" || change.Seq <= seed.Seq {
		t.Fatalf("the change: %+v", change)
	}
	must(db.Exec(ctx, "del "+name+" where t = $1", "old")).of(t)
	if del := <-events; !reflect.DeepEqual(del.Dels, []int64{1}) || len(del.Puts) != 0 {
		t.Fatalf("the deletion: %+v", del)
	}
	cancel()
	for range events {
	}
	if _, err := db.Subscribe(context.Background(), fresh("missing"), nil); statusOf(t, err).Status != 404 {
		t.Fatalf("a subscription to nothing: %v", err)
	}
}

func TestChangesHandsOverEveryWrite(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("cdc")
	before := must(db.Exec(ctx, "create collection "+name+" (t text)")).of(t)
	must(db.Exec(ctx, "put "+name+" [{t: $1}, {t: $2}]", "a", "b")).of(t)
	last := must(db.Exec(ctx, "del "+name+" where t = $1", "a")).of(t)

	var mine []fenecdb.Change
	since := before.Seq - 1
	for since < last.Seq {
		got := must(db.Changes(ctx, since, 5*time.Second)).of(t)
		for _, c := range got.Changes {
			if c.Collection == name {
				mine = append(mine, c)
			}
		}
		since = got.Next
	}
	var ops []string
	for _, c := range mine {
		ops = append(ops, c.Op+":"+fmt.Sprint(c.Doc["t"]))
	}
	if want := []string{"create:<nil>", "put:a", "put:b", "del:<nil>"}; !reflect.DeepEqual(ops, want) {
		t.Fatalf("the stream held %v", ops)
	}
	if mine[3].ID != 1 || mine[3].Seq != last.Seq {
		t.Fatalf("the deletion: %+v", mine[3])
	}
	// Nothing after the last write: the wait runs out with none.
	start := time.Now()
	empty := must(db.Changes(ctx, db.Seq(), 300*time.Millisecond)).of(t)
	if len(empty.Changes) != 0 || empty.Next != db.Seq() || time.Since(start) < 250*time.Millisecond {
		t.Fatalf("past the end: %+v after %v", empty, time.Since(start))
	}
	_, err := db.Changes(ctx, db.Seq()+1000, 0)
	if e := statusOf(t, err); e.Status != 409 {
		t.Fatalf("a cursor past the last write: %+v", e)
	}
}

func TestATenantPath(t *testing.T) {
	ctx := context.Background()
	dir := filepath.Join(scratch, "tenants")
	os.MkdirAll(dir, 0o700)
	node, stop := serve("--dir", dir, "--admin-token", "go-admin")
	defer stop()
	req, _ := http.NewRequest(http.MethodPut, node+"/_admin/tenants/acme", nil)
	req.Header.Set("Authorization", "Bearer go-admin")
	res, err := http.DefaultClient.Do(req)
	if err != nil || res.StatusCode != 201 {
		t.Fatalf("making the tenant: %v %v", res, err)
	}
	res.Body.Close()

	acme := fenecdb.New(node, fenecdb.WithTenant("acme"))
	must(acme.Exec(ctx, "create collection notes (t text)")).of(t)
	must(acme.Exec(ctx, "put notes {t: $1}", "acme's")).of(t)
	if rows := must(acme.Query(ctx, "get notes select t")).of(t); len(rows) != 1 || rows[0]["t"] != "acme's" {
		t.Fatalf("the tenant holds %v", rows)
	}
	if err := acme.Health(ctx); err != nil {
		t.Fatalf("the tenant's node is not healthy: %v", err)
	}
	_, err = fenecdb.New(node, fenecdb.WithTenant("nobody")).Query(ctx, "collections")
	if e := statusOf(t, err); e.Status != 404 {
		t.Fatalf("a tenant that is not there: %+v", e)
	}
}

func TestAScopedToken(t *testing.T) {
	ctx := context.Background()
	out, err := exec.Command(binary, "--jwt-secret", jwtSecret, "--mint-token", `{"sub":"alice"}`).Output()
	if err != nil {
		t.Fatal(err)
	}
	db := root()
	if _, err := db.Exec(ctx, "create collection notes (title text, owner text)"); err != nil {
		statusOf(t, err) // made by a test before: 409
	}
	must(db.Exec(ctx, "put notes {title: $1, owner: $2}", "bob's", "bob")).of(t)
	alice := fenecdb.New(primary, fenecdb.WithToken(strings.TrimSpace(string(out))))
	must(alice.Exec(ctx, "put notes {title: $1}", "alice's")).of(t)
	rows := must(alice.Query(ctx, "get notes select title, owner")).of(t)
	if len(rows) != 1 || rows[0]["owner"] != "alice" {
		t.Fatalf("alice reads %v", rows)
	}
	_, err = alice.Exec(ctx, "put notes {title: $1, owner: $2}", "forged", "bob")
	if e := statusOf(t, err); e.Status != 403 || e.Code != fenecdb.CodeForbidden {
		t.Fatalf("a write outside the policy: %+v", e)
	}
}

func TestHealth(t *testing.T) {
	// No token needed.
	if err := fenecdb.New(primary).Health(context.Background()); err != nil {
		t.Fatal(err)
	}
}

func TestASchemaIsComparedAndAppliedOnlyWhenAsked(t *testing.T) {
	ctx := context.Background()
	db := root()
	name := fresh("schema")
	v1 := "create collection " + name + " (title text required, n int @hash)"
	must(db.Exec(ctx, v1)).of(t)
	if plan := must(db.Schema(ctx, v1, nil, false)).of(t); len(plan.Refusals) != 0 {
		t.Fatalf("the same schema refused: %+v", plan)
	}
	// A field the server lacks: the server's to add.
	v2 := "create collection " + name + " (title text required, n int @hash, at timestamp)"
	_, err := db.Schema(ctx, v2, nil, false)
	var refused *fenecdb.SchemaError
	if !errors.As(err, &refused) || refused.Refusals[0].Kind != "field_missing" {
		t.Fatalf("a missing field answered %v", err)
	}
	// Asked, it is added; a rename is a migration, run once.
	if plan := must(db.Schema(ctx, v2, nil, true)).of(t); !plan.Applied || len(plan.Statements) != 1 {
		t.Fatalf("migrate answered %+v", plan)
	}
	v3 := "create collection " + name + " (name text required, n int @hash, at timestamp)"
	moved := []any{"alter collection " + name + " rename field title to name"}
	if _, err := db.Schema(ctx, v3, nil, true); !errors.As(err, &refused) {
		t.Fatalf("a rename unsaid answered %v", err)
	}
	if plan := must(db.Schema(ctx, v3, moved, true)).of(t); !plan.Ran || len(plan.Migrations) != 1 {
		t.Fatalf("the migration answered %+v", plan)
	}
	if plan := must(db.Schema(ctx, v3, moved, true)).of(t); plan.Applied || len(plan.Migrations) != 0 {
		t.Fatalf("again answered %+v", plan)
	}
	must(db.Exec(ctx, "drop collection _migrations")).of(t)
}
