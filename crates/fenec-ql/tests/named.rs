//! Named statements' syntax (`docs/design/named-statements.md`, slice 1):
//! every form read into its tree, the body's refusals that need nothing
//! but the text, the canonical tree's digest, and the engine refusing to
//! run any of it -- nothing of a definition or a call may land before the
//! slices that check and scope them.

use fenec_core::named::{Args, Call, Claim, Definition, Param, NOT_BUILT};
use fenec_core::prelude::*;
use fenec_ql::{parse, parse_exact, parse_one};

/// The design's ledger transfer, its guards' reasons aside (the test of
/// them below gives them).
const TRANSFER: &str = "create statement transfer($from int, $to int, $amount int)
  claims (sub text)
  comment 'Moves an amount from an account the caller owns to another'
  check $amount > 0 and $from != $to
as begin
  get accounts select id where id = $from and owner = $jwt.sub limit 1 require 1;
  set accounts {balance: balance - $amount} where id = $from and balance >= $amount require 1;
  set accounts {balance: balance + $amount} where id = $to require 1;
  insert entries {from: $from, to: $to, amount: $amount, by: $jwt.sub, at: now()}
    returning id, at
end";

fn definition(text: &str) -> Definition {
    match parse_one(text) {
        Ok(Statement::CreateStatement(d)) => *d,
        other => panic!("{text}: {other:?}"),
    }
}

fn call(text: &str) -> Call {
    match parse_one(text) {
        Ok(Statement::Call(c)) => *c,
        other => panic!("{text}: {other:?}"),
    }
}

/// The refusal of `text`, and where it is placed.
fn refused(text: &str) -> String {
    match parse(text) {
        Err(e) => e.to_string(),
        Ok(s) => panic!("{text} parsed: {s:?}"),
    }
}

/// `text` refused in words holding `words`, placed at the first `at` in
/// it: the word the refusal is about is not consumed.
fn refused_at(text: &str, words: &str, at: &str) {
    let place = text.find(at).expect("the word is in the text");
    refused_from(text, words, place);
}

/// `text` refused in words holding `words`, placed at byte `place` (in
/// characters, as the parser counts).
fn refused_from(text: &str, words: &str, place: usize) {
    let e = refused(text);
    assert!(e.contains(words), "{text}: {e}");
    let place = text[..place].chars().count();
    assert!(
        e.contains(&format!("position {place}:")),
        "{text}: {e}, not at {place}"
    );
}

#[test]
fn a_definition_reads_into_its_tree() {
    let d = definition(TRANSFER);
    assert_eq!(d.name, "transfer");
    assert!(!d.replace && d.block && !d.public && !d.scans);
    assert_eq!(
        d.params,
        ["from", "to", "amount"].map(|n| Param {
            name: n.into(),
            ty: DataType::Int,
            default: None,
        })
    );
    assert_eq!(
        d.claims,
        [Claim {
            name: "sub".into(),
            ty: DataType::Text,
            from: None,
        }]
    );
    assert_eq!(
        d.comment.as_deref(),
        Some("Moves an amount from an account the caller owns to another")
    );
    // The parameters are numbered in the signature's order, the claims
    // after them: the body holds the `Expr::Param`s every statement does.
    let want = Expr::And(
        Box::new(Expr::Cmp(
            CmpOp::Gt,
            Box::new(Expr::Param(2)),
            Box::new(Expr::Lit(Value::Int(0))),
        )),
        Box::new(Expr::Cmp(
            CmpOp::Ne,
            Box::new(Expr::Param(0)),
            Box::new(Expr::Param(1)),
        )),
    );
    assert_eq!(d.check, Some(want));
    assert_eq!(d.body.len(), 4);
    // `$jwt.sub` is the parameter past the three.
    let same =
        parse_one("get accounts select id where id = $1 and owner = $4 limit 1 require 1").unwrap();
    assert_eq!(d.body[0], same);
    let last = parse_one(
        "insert entries {from: $1, to: $2, amount: $3, by: $4, at: now()} returning id, at",
    )
    .unwrap();
    assert_eq!(d.body[3], last);
    // `$1` in a body is the first parameter, by place.
    let placed = definition(&TRANSFER.replace("where id = $to", "where id = $2"));
    assert_eq!(placed.body, d.body);
}

#[test]
fn the_signature_takes_defaults_claims_markers_and_a_body_of_one() {
    let d = definition(
        "create or replace statement page($n int = 10, $note text = null, $tags [text] = \
         ['a'], $at timestamp) claims (groups [text] from \"cognito:groups\", plan text from \
         'https://example.com/plan', sub text from \"sub\") scans as get notes select id, \
         body where tags has $jwt.groups order id limit $n",
    );
    assert!(d.replace && !d.block && d.scans);
    assert_eq!(d.params[0].default, Some(Value::Int(10)));
    assert_eq!(d.params[1].default, Some(Value::Null));
    assert_eq!(
        d.params[2].default,
        Some(Value::List(vec![Value::Text("a".into())]))
    );
    assert_eq!(d.params[2].ty, DataType::List(Box::new(DataType::Text)));
    assert_eq!(d.params[3].default, None);
    assert_eq!(d.claims[0].claim(), "cognito:groups");
    assert_eq!(d.claims[1].claim(), "https://example.com/plan");
    // A claim named as the token names it is none.
    assert_eq!(d.claims[2].from, None);
    let Statement::Select(s) = &d.body[0] else {
        panic!()
    };
    assert_eq!(s.limit, Some(Count::Param(0)));
    assert!(matches!(&s.filter, Some(Expr::Has(_, p)) if **p == Expr::Param(4)));
    // `public`, a writing body of one statement, its `;` the text's.
    let d = definition(
        "create statement vote($poll int) public as set polls {votes: votes + 1} where id = \
         $poll;",
    );
    assert!(d.public && d.claims.is_empty() && d.writes());
    // The clauses in any order, `claims` before the `check` reading them;
    // `begin ... end` with a `;` before its `end`.
    let d = definition(
        "create statement s($a int) comment 'x' claims (sub text) check $jwt.sub != '' as \
         begin get t select a where a = $a; end",
    );
    assert!(d.block && d.body.len() == 1 && d.check.is_some());
    // Statements after a definition are statements of their own.
    let two = parse("create statement s() as get t select a\nget u").unwrap();
    assert!(matches!(
        &two[..],
        [Statement::CreateStatement(_), Statement::Select(_)]
    ));
}

#[test]
fn drop_statements_and_describe_read_as_their_own() {
    assert_eq!(
        parse_one("drop statement transfer").unwrap(),
        Statement::DropStatement {
            name: "transfer".into(),
            if_exists: false
        }
    );
    assert_eq!(
        parse_one("DROP STATEMENT IF EXISTS transfer").unwrap(),
        Statement::DropStatement {
            name: "transfer".into(),
            if_exists: true
        }
    );
    assert_eq!(parse_one("statements").unwrap(), Statement::ListStatements);
    assert_eq!(
        parse_one("describe statement transfer").unwrap(),
        Statement::DescribeStatement("transfer".into())
    );
    // `describe statement` alone is the collection `statement`'s, as it
    // always was, and so is it before a statement.
    assert_eq!(
        parse_one("describe statement").unwrap(),
        Statement::Describe("statement".into())
    );
    let two = parse("describe statement get t").unwrap();
    assert_eq!(two[0], Statement::Describe("statement".into()));
    assert!(matches!(two[1], Statement::Select(_)));
}

#[test]
fn a_call_reads_in_each_of_its_forms() {
    let lit = |n: i64| Expr::Lit(Value::Int(n));
    let named = |pairs: &[(&str, Expr)]| {
        Args::Named(
            pairs
                .iter()
                .map(|(k, e)| (k.to_string(), e.clone()))
                .collect(),
        )
    };
    assert_eq!(
        call("call purge"),
        Call {
            name: "purge".into(),
            args: Args::None,
            claims: None
        }
    );
    assert_eq!(call("CALL purge()"), call("call purge"));
    let doc = call("call transfer {from: 1, to: 2, amount: 300}");
    assert_eq!(
        doc.args,
        named(&[("from", lit(1)), ("to", lit(2)), ("amount", lit(300))])
    );
    // SQL's arguments by name are the document's.
    assert_eq!(
        call("CALL transfer(from => 1, to => 2, amount => 300)"),
        doc
    );
    assert_eq!(
        call("CALL transfer(1, 2, amount => 300)").args,
        Args::Placed(vec![lit(1), lit(2)], vec![("amount".into(), lit(300))])
    );
    assert_eq!(call("call transfer $1").args, Args::Param(0));
    let c = call("call transfer $1 as {sub: 'alice'}");
    assert_eq!(
        c.claims,
        Some(named(&[("sub", Expr::Lit(Value::Text("alice".into())))]))
    );
    assert_eq!(
        call("call transfer {from: $1} as $2").claims,
        Some(Args::Param(1))
    );
    assert_eq!(parse_one("call transfer $1 as $2").unwrap().max_param(), 2);
    // An expression of values is a value.
    assert_eq!(
        call("CALL f(1 + $1)").args,
        Args::Placed(
            vec![Expr::Arith(
                fenec_core::query::ArithOp::Add,
                Box::new(lit(1)),
                Box::new(Expr::Param(0))
            )],
            vec![]
        )
    );
}

/// A definition's numbered names, and a name after `$` outside one.
#[test]
fn named_parameters_are_a_definition_s_alone() {
    refused_at(
        "get t where id = $from",
        "`$from` names a named statement's parameter; a statement sent alone numbers them, `$1`",
        "$from",
    );
    refused_at(
        "get t where owner = $jwt.sub",
        "`$jwt.sub` names a named statement's parameter",
        "$jwt",
    );
    refused_at(
        "put t $docs",
        "names a named statement's parameter",
        "$docs",
    );
    refused_at(
        "get t limit $n",
        "names a named statement's parameter",
        "$n",
    );
    refused_at(
        "call x $args",
        "names a named statement's parameter",
        "$args",
    );
    let def = "create statement s($a int) claims (sub text) as get t select a where ";
    refused_at(
        &format!("{def}a = $b"),
        "`$b` names no parameter the statement declares",
        "$b",
    );
    refused_at(
        &format!("{def}a = $jwt.email"),
        "`$jwt.email` names no claim the statement declares: `claims (email text)`",
        "$jwt.email",
    );
    refused_at(
        &format!("{def}a = $jwt"),
        "`$jwt` is the caller's token",
        "$jwt",
    );
    refused_at(
        &format!("{def}a = $jwt.sub.x"),
        "a claim is read whole, `$jwt.sub`",
        "$jwt.sub.x",
    );
    refused_at(
        &format!("{def}a = $a.x"),
        "a parameter is read whole",
        "$a.x",
    );
    // `$2` would be the claim: a claim is read by its name alone.
    refused_at(
        &format!("{def}a = $2"),
        "`$2`: the statement declares 1 parameters",
        "$2",
    );
    // After the definition the names are gone again.
    let two = "create statement s($a int) as get t select a where a = $a; get t where a = $a";
    refused_from(
        two,
        "names a named statement's parameter",
        two.rfind("$a").unwrap(),
    );
}

#[test]
fn the_signature_s_refusals() {
    refused_at(
        "create statement \"my statement\"() as get t select a",
        "`my statement` is no statement's name",
        "\"my",
    );
    refused_at(
        "create statement şehir() as get t select a",
        "no statement's name",
        "şehir",
    );
    refused_at(
        "create statement get() as get t select a",
        "`get` is no statement's name",
        "get()",
    );
    let long = "a".repeat(60);
    refused_at(
        &format!("create statement {long}() as get t select a"),
        "at most 59 bytes",
        &long,
    );
    refused_at(
        "create statement s as get t",
        "follow its name in parentheses",
        "as get",
    );
    refused_at(
        "create statement s(a int) as get t select a",
        "a parameter is named with its `$`",
        "a int",
    );
    refused_at(
        "create statement s($jwt text) as get t select a",
        "`$jwt` is the caller's token",
        "$jwt",
    );
    refused_at(
        "create statement s($a int, $a text) as get t select a",
        "`$a` is declared twice",
        "$a text",
    );
    refused_at(
        "create statement s($a int = now()) as get t select a",
        "a parameter's default is a literal",
        "now",
    );
    refused_at(
        "create statement s() claims () as get t select a",
        "`claims` declares the claims the body reads",
        ") as",
    );
    refused_at(
        "create statement s() claims (sub text, user text from \"sub\") as get t select a",
        "the claim `sub` is declared twice",
        "user",
    );
    refused_at(
        "create statement s() claims (sub text) claims (sub text) as get t select a",
        "`claims` is written once",
        "claims (sub text) as",
    );
    refused_at(
        "create statement s($a int) check $a > 0 claims (sub text) as get t select a",
        "`claims` goes before `check`",
        "claims",
    );
    refused_at(
        "create statement s() comment '' as get t select a",
        "`comment` takes what the statement does",
        "''",
    );
    refused_at(
        "create statement s() every 1h as get t select a",
        "a statement's body follows `as`",
        "every",
    );
    refused_at(
        "create statement s($a int) check a > 0 as get t select a",
        "`check` reads the parameters, the claims and `now()`, and no field: `a`",
        "a > 0",
    );
    refused_at(
        "create statement s($a int) check $a in (get u select id) as get t select a",
        "`check` reads no collection",
        "$a in (",
    );
    let params: Vec<String> = (0..33).map(|i| format!("$p{i} int")).collect();
    refused_at(
        &format!(
            "create statement s({}) as get t select a",
            params.join(", ")
        ),
        "at most 32 parameters",
        "$p32",
    );
    let claims: Vec<String> = (0..17).map(|i| format!("c{i} text")).collect();
    refused_at(
        &format!(
            "create statement s() claims ({}) as get t select a",
            claims.join(", ")
        ),
        "at most 16 claims",
        "c16",
    );
    let body: Vec<&str> = vec!["get t select a"; 65];
    refused_at(
        &format!("create statement s() as begin {} end", body.join("; ")),
        "a body holds at most 64 statements",
        "get t select a end",
    );
    refused_at(
        "create statement s() as begin end",
        "at least one statement",
        "s()",
    );
}

#[test]
fn a_body_holds_reads_inserts_and_guarded_writes_alone() {
    let def = "create statement s($id int, $doc json) claims (sub text) as ";
    let body = |b: &str| format!("{def}{b}");
    // Each refused at the statement, with the form to write.
    for (b, words) in [
        ("put t {id: $id, owner: $jwt.sub}", "a `put` naming an id writes over"),
        ("insert t $doc", "`insert <collection> $n` writes the caller's documents"),
        ("put t $doc", "`insert <collection> $n` writes the caller's documents"),
        (
            "insert t {k: $id, by: $jwt.sub} if absent else set {n: n + 1}",
            "`if absent else set` finds its row by a key",
        ),
        (
            "insert into t (k, by) values ($id, $jwt.sub) on conflict (k) do update set n = t.n + 1",
            "`if absent else set` finds its row by a key",
        ),
        ("set t {by: $jwt.sub}", "say which rows they write"),
        ("del t returning id", "say which rows they write"),
        ("create collection x (a int)", "a body changes no schema"),
        ("drop collection t", "a body changes no schema"),
        ("alter collection t add field b int", "a body changes no schema"),
        ("create index on t (a) @hash", "a body changes no schema"),
        ("drop statement x", "a body changes no schema"),
        ("compact t", "a body compacts nothing"),
        ("call other {id: $id}", "a body calls no statement"),
        ("collections", "a body holds `get`, `insert`"),
        ("explain get t select a", "a body holds `get`, `insert`"),
    ] {
        refused_from(&body(b), words, def.len());
    }
    refused_at(
        &body("create statement inner() as get t select a"),
        "a body defines no statement",
        "statement inner",
    );
    // A body over several: the refusal is the statement's that holds it.
    refused_at(
        &body("begin get t select a where id = $id; put t {id: $id, by: $jwt.sub} end"),
        "a `put` naming an id writes over",
        "put t",
    );
    // `where true` says every row is meant; `order ... limit` picks.
    definition("create statement purge() public as del t where true");
    definition(&body("set t {by: $jwt.sub} order at limit 1"));
    // `put ... if absent` never writes over: it is `insert ... if absent`.
    assert_eq!(
        definition(&body("put t {k: $id, by: $jwt.sub} if absent")).body,
        definition(&body("insert t {k: $id, by: $jwt.sub} if absent")).body
    );
}

#[test]
fn a_body_names_no_collection_of_the_database_s_own() {
    let def = "create statement s($id int) claims (sub text) as ";
    for (b, name) in [
        ("get _consumers select name", "_consumers"),
        ("get _follow select at", "_follow"),
        ("insert _idempotency {key: $jwt.sub}", "_idempotency"),
        ("set _sync_queue {at: 1} where id = $id", "_sync_queue"),
        ("del _migrations where id = $id", "_migrations"),
        (
            "get t select a where a in (get _idempotency select key)",
            "_idempotency",
        ),
        (
            "get t select a lookup _consumers on name select at",
            "_consumers",
        ),
        (
            "set t {a: 1} where a in (get u select id where id in (get _follow select at))",
            "_follow",
        ),
        (
            "select o.id, c.at from orders o join _follow c on c.id = o.f",
            "_follow",
        ),
    ] {
        refused_from(
            &format!("{def}{b}"),
            &format!("`{name}` is the database's own"),
            def.len(),
        );
    }
}

#[test]
fn the_answer_names_its_fields() {
    let def = "create statement s($id int) claims (sub text) as ";
    for b in [
        "get t",
        "get t where id = $id",
        "select * from t",
        "get t select a lookup u on t_id",
        "set t {by: $jwt.sub} where id = $id returning *",
        "insert t {by: $jwt.sub} returning *",
        "select o.* from orders o join customers c on c.id = o.customer_id",
    ] {
        refused_from(
            &format!("{def}{b}"),
            "names every field it answers",
            def.len(),
        );
    }
    // The statements before the last answer nothing, and a list is theirs
    // to leave out; a count, a fold and `returning` a list name theirs.
    for b in [
        "begin get t where id = $id require 1; get t select a, b end",
        "get t where owner = $jwt.sub count",
        "get t select count(*) as n where owner = $jwt.sub",
        "get t select a lookup u on t_id select b",
        "set t {by: $jwt.sub} where id = $id",
        "set t {by: $jwt.sub} where id = $id returning id, by",
        "select o.id, c.name from orders o join customers c on c.id = o.customer_id",
    ] {
        definition(&format!("{def}{b}"));
    }
}

#[test]
fn a_writing_body_reads_its_caller_or_says_it_is_public() {
    refused_at(
        "create statement vote($poll int) as set polls {votes: votes + 1} where id = $poll",
        "statement `vote` writes and reads no claim",
        "vote(",
    );
    // A claim read by `check` alone reads the caller.
    definition(
        "create statement vote($poll int) claims (role text) check $jwt.role = 'admin' as \
         set polls {votes: votes + 1} where id = $poll",
    );
    // A read needs neither.
    definition(
        "create statement board() as get scores select name, score order score desc limit 10",
    );
    refused_at(
        "create statement vote($poll int) claims (sub text) public as set polls {by: $jwt.sub} \
         where id = $poll",
        "`public` says the body writes on behalf of nobody",
        "vote(",
    );
}

/// The SQL spellings with no meaning of named statements' are refused
/// naming the FenecQL to write, at the word, not consumed.
#[test]
fn sql_s_routines_are_refused_naming_named_statements() {
    refused_at(
        "SELECT transfer(1, 2, 300)",
        "`transfer(...)` names no collection: a function reads a collection's rows, `select \
         transfer(...) from <collection>`, and a named statement runs with `call transfer(...)`",
        "transfer",
    );
    refused_at(
        "SELECT * FROM transfer(1, 2, 300)",
        "`transfer(...)` names no collection: a routine's rows are no table here, and a named \
         statement runs with `call transfer(...)`",
        "transfer",
    );
    // A list's function with its `from` left out is told both.
    refused_at(
        "select count(*) where n > 1",
        "`select count(...) from <collection>`",
        "count",
    );
    for text in [
        "CREATE FUNCTION transfer(a int) RETURNS void AS 'select 1'",
        "create procedure transfer(a int) language sql as 'select 1'",
        "CREATE OR REPLACE FUNCTION transfer() RETURNS int",
    ] {
        let word = text
            .split_whitespace()
            .find(|w| w.eq_ignore_ascii_case("function") || w.eq_ignore_ascii_case("procedure"))
            .unwrap();
        let e = refused(text);
        assert!(
            e.contains("a routine here is a named statement, `create statement"),
            "{e}"
        );
        let place = text.find(word).unwrap();
        assert!(e.contains(&format!("position {place}:")), "{text}: {e}");
    }
    refused_at(
        "DROP FUNCTION transfer",
        "a named statement is dropped with `drop statement <name>`",
        "FUNCTION",
    );
    refused_at(
        "create or replace table t (a int)",
        "`create or replace` defines a named statement",
        "table",
    );
    // A call's arguments are values.
    refused_at("CALL f(a)", "an argument is a value", "a)");
    refused_at("call f {a: b}", "`b` names a field", "{");
    refused_at("call f {a: 1, a: 2}", "`a` is given twice", "{");
    refused_at(
        "CALL f(a => 1, 2)",
        "an argument by place goes before",
        "2)",
    );
    refused_at("call f.g", "`f.g` is a path", "f.g");
    refused_at(
        "call f as sub",
        "`as` takes the claims a call is made with",
        "sub",
    );
}

/// The design's transfer as written, each guard saying why: `require <n>
/// else '<reason>'` reads in a body as it does alone, the reason in the
/// tree, and a reason is part of what the digest holds.
#[test]
fn a_body_s_guards_say_why() {
    let reasons = TRANSFER
        .replacen(
            "limit 1 require 1",
            "limit 1 require 1 else 'not your account'",
            1,
        )
        .replacen(
            "balance >= $amount require 1",
            "balance >= $amount\n    require 1 else 'insufficient funds'",
            1,
        )
        .replacen(
            "id = $to require 1",
            "id = $to require 1 else 'no such account'",
            1,
        );
    let d = definition(&reasons);
    let said: Vec<&str> = d
        .body
        .iter()
        .filter_map(|s| match s {
            Statement::Select(s) => s.require.as_ref(),
            Statement::Update { require, .. } => require.as_ref(),
            _ => None,
        })
        .map(|r| &*r.reason)
        .collect();
    assert_eq!(
        said,
        ["not your account", "insufficient funds", "no such account"]
    );
    // The body's statement is the one sent alone, numbered as the
    // signature numbers it.
    let alone = parse_one(
        "set accounts {balance: balance - $3} where id = $1 and balance >= $3 require 1 else \
         'insufficient funds'",
    )
    .unwrap();
    assert_eq!(d.body[1], alone);
    assert_ne!(d.digest().unwrap(), definition(TRANSFER).digest().unwrap());
}

/// `describe statement` at the text's end is the collection `statement`'s
/// description, and a name after it makes it a named statement's: the
/// reading says the end decided it, so the shell holds it for its next
/// line rather than run it at Enter.
#[test]
fn describe_statement_at_the_end_waits_for_its_name() {
    use fenec_ql::{reading, Rest};
    assert!(reading("DESCRIBE STATEMENT -- its name next\n").open);
    let r = reading("describe statement");
    assert!(r.open, "{r:?}");
    let Rest::Last(0, s) = &r.rest else {
        panic!("{r:?}")
    };
    assert_eq!(**s, Statement::Describe("statement".into()));
    let r = reading("describe statement\ntransfer");
    assert!(!r.open, "{r:?}");
    let Rest::Last(0, s) = &r.rest else {
        panic!("{r:?}")
    };
    assert_eq!(**s, Statement::DescribeStatement("transfer".into()));
    for whole in [
        "describe statement transfer",
        "describe notes",
        "describe statement;",
        "get notes count",
        "get notes count\n",
    ] {
        assert!(!reading(whole).open, "{whole}");
    }
}

/// `end` closes a body: a name SQL writes without `as` takes it nowhere in
/// one, and anywhere outside one as before.
#[test]
fn end_closes_a_body_and_is_an_alias_outside_one() {
    let d = definition(
        "create statement s($id int) as begin select a from t where id = $id; select a from t \
         end",
    );
    assert_eq!(d.body.len(), 2);
    // Outside a body, `end` after `from t` is the table's alias, as it was.
    let s = parse_one("select a from t end").unwrap();
    assert!(matches!(s, Statement::Select(_)));
    // A body cut short of its `end` is refused at the text's end.
    let e = refused("create statement s() as begin get t select a");
    assert!(e.contains("a body's statements are cut by `;`"), "{e}");
    let e = refused("create statement s() as begin get t select a;");
    assert!(e.contains("ends with `end`; found end of input"), "{e}");
}

/// The canonical tree's digest: the same for every text that parses to the
/// same definition, another for every change of meaning.
#[test]
fn the_digest_holds_the_tree_not_the_text() {
    let digest = |t: &str| definition(t).digest().unwrap();
    let base = digest(TRANSFER);
    // White space, comments, keywords' case, SQL's spellings, `create or
    // replace`, `begin ... end` around one statement.
    let spelled = "CREATE OR REPLACE STATEMENT transfer( $from int,$to int , $amount int ) -- a ledger
  CLAIMS (sub TEXT)  COMMENT \"Moves an amount from an account the caller owns to another\"
  CHECK ($amount > 0) AND $from <> $to # guards
AS BEGIN
  SELECT id FROM accounts WHERE id = $1 AND owner = $jwt.sub LIMIT 1 REQUIRE 1 ;
  UPDATE accounts SET balance = balance - $amount WHERE id = $from AND balance >= $amount REQUIRE 1;;
  update accounts set {balance: balance + $amount} where id = $to require 1;
  INSERT INTO entries (from, to, amount, by, at) VALUES ($from, $to, $amount, $jwt.sub, now())
    RETURNING id, at;
END";
    assert_eq!(digest(spelled), base);
    assert_eq!(
        digest("create statement s($a int) claims (sub text) as get t select a where a = $a"),
        digest(
            "create statement s($a int) claims (sub text) as begin get t select a where a = \
             $a; end"
        )
    );
    // A list of numbers reads the same either way the server parses it.
    let exact = |t: &str| match parse_exact(t).unwrap().remove(0) {
        Statement::CreateStatement(d) => d.digest().unwrap(),
        _ => panic!(),
    };
    assert_eq!(exact(TRANSFER), base);
    // Every change of meaning moves it.
    let mut seen = vec![base];
    for (from, to) in [
        ("transfer(", "transfers("),
        ("$amount int)", "$amount float)"),
        ("$amount int)", "$amount int = 1)"),
        ("claims (sub text)", "claims (sub text from \"uid\")"),
        ("claims (sub text)", "claims (sub [text])"),
        ("claims (sub text)", "claims (sub text) scans"),
        ("caller owns", "caller holds"),
        ("$amount > 0", "$amount > 1"),
        ("$amount > 0", "$amount >= 0"),
        ("limit 1 require 1", "limit 1"),
        (
            "limit 1 require 1",
            "limit 1 require 1 else 'not your account'",
        ),
        ("limit 1 require 1", "limit 1 require 1 else 'not yours'"),
        ("balance >= $amount", "balance > $amount"),
        ("where id = $to", "where id = $from"),
        ("returning id, at", "returning id"),
        ("returning id, at", "returning at, id"),
        ("by: $jwt.sub", "by: $from"),
        ("at: now()", "at: 0"),
        ("insert entries", "insert ledger"),
    ] {
        assert!(TRANSFER.contains(from), "{from}");
        let d = digest(&TRANSFER.replacen(from, to, 1));
        assert!(!seen.contains(&d), "{from} -> {to}");
        seen.push(d);
    }
    // A parameter's name is the API's, though the body reads it by place.
    let renamed = digest(&TRANSFER.replace("$from", "$src"));
    assert!(!seen.contains(&renamed));
    // A literal's bits: `0.0` and `-0.0` compare equal, and write apart.
    let lit = |v: &str| {
        digest(&format!(
            "create statement s() public as set t {{x: {v}}} where id = 1"
        ))
    };
    assert_ne!(lit("0.0"), lit("-0.0"));
    assert_ne!(lit("1"), lit("1.0"));
    assert_ne!(lit("'1'"), lit("1"));
    // Pinned: a change to the encoding that moves this moves every stored
    // statement's, and raises fenec_ql::GRAMMAR_EPOCH with it.
    assert_eq!(base, 0x45b7_05e6_1089_ee98, "{base:#018x}");
    assert_eq!(fenec_ql::GRAMMAR_EPOCH, 1);
}

/// A field added to the tree that a statement does not use leaves its
/// digest as it was: the encoding writes a field only where it holds
/// something. Shown by the parts a definition leaves empty.
#[test]
fn a_field_that_holds_nothing_is_not_written() {
    let d = definition("create statement s() as get t select a");
    let bytes = d.canonical().unwrap();
    // The definition, its name, its body of one `get` of `t` naming `a`.
    let mut want = vec![1, 1, 1, b's', 8, 1, 1, 1, 1, 1, b't', 2, 1, 1, b'a', 0, 0];
    assert_eq!(bytes, want);
    let d = definition("create statement s() public as get t select a");
    want.splice(4..4, [4]);
    assert_eq!(d.canonical().unwrap(), want);
}

/// Nothing runs a definition, a drop or a call yet: each is refused in the
/// words of its own before anything of it -- or of a block holding it --
/// lands. `statements` lists none, and `describe statement` finds none.
#[test]
fn the_engine_refuses_to_run_them() {
    let mut db = Database::new();
    for s in parse("create collection t (a int); put t {a: 1}").unwrap() {
        db.execute(&s).unwrap();
    }
    let not_built = Error::Query(NOT_BUILT.into());
    for text in [
        "create statement s() as get t select a",
        "create or replace statement s() as get t select a",
        "drop statement s",
        "drop statement if exists s",
        "call s",
        "call s {a: 1}",
        "call s $1 as {sub: 'x'}",
        "CALL s(1, b => 2)",
    ] {
        let s = parse_one(text).unwrap();
        assert!(!s.is_read_only() && s.is_named(), "{text}");
        assert_eq!(
            db.execute_with(&s, &[Value::Int(1)]),
            Err(not_built.clone()),
            "{text}"
        );
        assert_eq!(db.query(&s, &[]), Err(not_built.clone()), "{text}");
        // In a block, the block is refused before its first statement runs.
        let put = parse_one("put t {a: 2}").unwrap();
        let r = db.execute_block(&[(&put, &[]), (&s, &[])]);
        assert_eq!(r, Err((1, not_built.clone())), "{text}");
    }
    let all = db
        .query(&parse_one("get t select a").unwrap(), &[])
        .unwrap();
    assert_eq!(all.rows().unwrap().rows.len(), 1);
    assert_eq!(db.collection_names(), ["t"]);
    // The reads answer for a database that holds none.
    for run in [true, false] {
        let list = parse_one("statements").unwrap();
        let r = match run {
            true => db.query(&list, &[]),
            false => db.execute(&list),
        };
        let rows = r.unwrap();
        assert_eq!(rows.rows().unwrap().rows.len(), 0);
        let describe = parse_one("describe statement transfer").unwrap();
        let e = match run {
            true => db.query(&describe, &[]),
            false => db.execute(&describe),
        };
        assert_eq!(e, Err(Error::NotFound("statement `transfer`".into())));
    }
}

/// A `Statement` is as large as its largest variant, a `get`'s: the new
/// ones are boxed or small.
#[test]
fn a_statement_is_as_large_as_a_select() {
    use std::mem::size_of;
    assert_eq!(
        size_of::<Statement>(),
        size_of::<fenec_core::query::Select>() + size_of::<usize>()
    );
}
