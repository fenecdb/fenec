using System.Collections;
using System.Globalization;
using System.Reflection;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FenecDb;

// The query builder: FenecQL text and its parameters from a chain of calls.
//
//     var rows = await db.From("docs")
//         .Select("title")
//         .Where("year", ">=", 2024)
//         .Near("embed", vector, ef: 64)
//         .Limit(5)
//         .RowsAsync();
//
// It makes the text web/fenec.js's builder makes of the same chain, to the
// byte, and so do the Python and Go builders: integrations/builder-golden.json
// holds the chains and what each must make, and every builder's tests run
// it. Every value goes in as a parameter; a name -- a collection, a field, a
// path into a json field -- cannot, so names are checked against FenecQL's
// own rule, and that check is the injection boundary.

/// <summary>A statement the query builder refused before sending anything: a name that is not one, an operator
/// it does not know, clauses that do not go together. The message is the JS builder's, word for word.</summary>
public sealed class FenecQueryException(string message) : ArgumentException(message);

/// <summary>One key of a lookup's order: a field, <c>asc</c> or <c>desc</c>, and a collation (<c>tr</c> or
/// <c>und</c>). A string is an ascending key.</summary>
public readonly record struct SortKey(string Field, string Direction = "asc", string? Collate = null)
{
    /// <summary>An ascending key.</summary>
    public static implicit operator SortKey(string field) => new(field);
}

/// <summary>
/// A condition: what <see cref="Or"/>, <see cref="And"/>, <see cref="Not"/>, <see cref="Raw"/>,
/// <see cref="Cmp"/> and <see cref="Fields"/> make. A <c>Dictionary&lt;string, object?&gt;</c> is one too, the JS
/// builder's object condition: each field's value is equality, <c>null</c> is <c>is null</c>, and a dictionary
/// is its operators -- <c>new() { ["year"] = new Dictionary&lt;string, object?&gt; { ["gte"] = 2024 } }</c> --
/// in the order written, which is the text's.
/// </summary>
public sealed class Cond
{
    internal readonly Node Node;

    internal Cond(Node node) => Node = node;

    /// <summary>The fields' conditions, joined with <c>and</c>.</summary>
    public static implicit operator Cond(Dictionary<string, object?> fields) => Fields(fields);

    /// <summary>Joins conditions with <c>or</c>.</summary>
    public static Cond Or(params Cond[] conds) => new(new Node("or") { Items = conds.Select(c => c.Node).ToList() });

    /// <summary>Joins conditions with <c>and</c>: Where already ands, so this is only needed inside Or.</summary>
    public static Cond And(params Cond[] conds) => new(new Node("and") { Items = conds.Select(c => c.Node).ToList() });

    /// <summary>Negates a condition.</summary>
    public static Cond Not(Cond cond) => new(new Node("not") { Items = [cond.Node] });

    /// <summary>Everything the builder cannot express (a function call): each <c>?</c> is bound to the next
    /// parameter -- a literal <c>?</c> goes in as one too. <c>Cond.Raw("cosine(embed, ?) &gt; ?", vec, 0.5)</c></summary>
    public static Cond Raw(string sql, params object?[] parameters) =>
        new(new Node("raw") { Sql = sql, Values = parameters.ToList() });

    /// <summary>One comparison, Where's three arguments as a condition.</summary>
    public static Cond Cmp(string field, string op, object? value) => new(Builder.CondOf(field, op, value));

    /// <summary>The JS builder's object condition: each field held to its spec, joined with <c>and</c>.</summary>
    public static Cond Fields(IEnumerable<KeyValuePair<string, object?>> fields)
    {
        var items = fields.Select(kv => Builder.FieldCond(Builder.FieldPath(kv.Key), kv.Value)).ToList();
        return new(items.Count == 1 ? items[0] : new Node("and") { Items = items });
    }
}

/// <summary>
/// A value worked out over the row a write writes: what <see cref="Inc"/> and <see cref="Expr"/> make, rendered as
/// FenecQL with its values as parameters.
/// </summary>
public sealed class Computed
{
    internal readonly string Kind;
    internal readonly object? By;
    internal readonly string Sql = "";
    internal readonly List<object?> Values = [];

    Computed(string kind, object? by, string sql, List<object?> values) =>
        (Kind, By, Sql, Values) = (kind, by, sql, values);

    /// <summary>A field plus <paramref name="by"/> in an update, counting from 0 where it is null --
    /// <c>["n"] = Computed.Inc(1)</c> is <c>n: coalesce(n, 0) + $1</c> -- worked out under the server's write
    /// lock, so increments from many clients all land.</summary>
    public static Computed Inc(object? by = null)
    {
        by ??= 1L;
        if (!FiniteNumber(by)) throw Builder.Refuse($"inc() takes a number: {Builder.JsJson(by)}");
        return new("inc", by, "", []);
    }

    /// <summary>A value as a FenecQL expression over the row it is written into, each <c>?</c> bound to the next
    /// parameter: <c>Computed.Expr("now()")</c>, <c>Computed.Expr("price * ?", 1.2)</c>.</summary>
    public static Computed Expr(string sql, params object?[] parameters) => new("expr", null, sql, parameters.ToList());

    static bool FiniteNumber(object v) => v switch
    {
        sbyte or byte or short or ushort or int or uint or long or ulong or decimal => true,
        float f => float.IsFinite(f),
        double d => double.IsFinite(d),
        _ => false,
    };

    internal string Render(string name, Binder bind, string write)
    {
        if (Kind == "inc")
        {
            if (write == "insert") throw Builder.Refuse($"inc() reads the row it changes: use it in update (field: {name})");
            return $"coalesce({name}, 0) + {bind.Bind(By)}";
        }
        var pieces = Sql.Split('?');
        var out_ = new StringBuilder();
        for (var i = 0; i < pieces.Length - 1; i++)
        {
            if (i >= Values.Count) throw Builder.Refuse("expr(): more `?` placeholders than parameters");
            out_.Append(pieces[i]).Append(bind.Bind(Values[i]));
        }
        if (pieces.Length - 1 != Values.Count) throw Builder.Refuse("expr(): too many parameters given");
        return out_.Append(pieces[^1]).ToString();
    }
}

internal sealed class Node(string t)
{
    public string T { get; } = t; // and, or, not, null, in, cmp, raw
    public List<Node> Items { get; init; } = [];
    public string Field { get; init; } = "";
    public string Op { get; init; } = "";
    public object? Value { get; init; }
    public List<object?> Values { get; init; } = [];
    public bool Negated { get; init; }
    public string Sql { get; init; } = "";
}

internal static class Builder
{
    public const int MaxLookupDepth = 8;

    static readonly Dictionary<string, string> Ops = new(StringComparer.Ordinal)
    {
        ["="] = "=", ["eq"] = "=",
        ["!="] = "!=", ["ne"] = "!=", ["neq"] = "!=",
        ["<"] = "<", ["lt"] = "<",
        ["<="] = "<=", ["lte"] = "<=", ["le"] = "<=",
        [">"] = ">", ["gt"] = ">",
        [">="] = ">=", ["gte"] = ">=", ["ge"] = ">=",
        ["~"] = "~", ["like"] = "~", ["contains"] = "~",
        ["has"] = "has",
        ["in"] = "in",
    };

    public static FenecQueryException Refuse(string message) => new(message);

    // A dictionary's entries in its order: through IEnumerable a generic one hands out KeyValuePairs, which no
    // cast makes DictionaryEntries.
    static IEnumerable<DictionaryEntry> Entries(IDictionary dict)
    {
        var e = dict.GetEnumerator();
        while (e.MoveNext()) yield return e.Entry;
    }

    // FenecQL's identifier: a Unicode letter or _, then letters, digits and _, as the lexer reads it --
    // JavaScript's \p{Alphabetic} and \p{N}. .NET has no Alphabetic property, which is the letters, the letter
    // numbers and the marks of Other_Alphabetic (a Devanagari vowel sign): after the first character every mark
    // is taken, a name the lexer would refuse being refused by the server rather than let through as text.
    static bool IsIdent(string s)
    {
        if (s.Length == 0) return false;
        var first = true;
        for (var i = 0; i < s.Length;)
        {
            if (Rune.DecodeFromUtf16(s.AsSpan(i), out var r, out var n) != System.Buffers.OperationStatus.Done)
                return false;
            var cat = Rune.GetUnicodeCategory(r);
            var start = r.Value == '_' || cat is UnicodeCategory.UppercaseLetter or UnicodeCategory.LowercaseLetter
                or UnicodeCategory.TitlecaseLetter or UnicodeCategory.ModifierLetter or UnicodeCategory.OtherLetter
                or UnicodeCategory.LetterNumber;
            var more = cat is UnicodeCategory.DecimalDigitNumber or UnicodeCategory.OtherNumber
                or UnicodeCategory.NonSpacingMark or UnicodeCategory.SpacingCombiningMark;
            if (!(start || (!first && more))) return false;
            first = false;
            i += n;
        }
        return true;
    }

    public static string Ident(string? name, string what = "field") =>
        name is not null && IsIdent(name) ? name : throw Refuse($"invalid {what} name: {Quote(name)}");

    /// <summary>A field's name, or a path into a json field: <c>meta.lang</c>.</summary>
    public static string FieldPath(string? name) =>
        name is not null && name.Split('.').All(IsIdent) ? name : throw Refuse($"invalid field name: {Quote(name)}");

    // A name as JSON.stringify writes it, which the messages quote with: System.Text.Json escapes every
    // character outside ASCII unless told otherwise.
    public static string Quote(string? s)
    {
        if (s is null) return "null";
        var b = new StringBuilder("\"");
        foreach (var c in s)
            b.Append(c switch
            {
                '"' => "\\\"",
                '\\' => "\\\\",
                '\b' => "\\b",
                '\f' => "\\f",
                '\n' => "\\n",
                '\r' => "\\r",
                '\t' => "\\t",
                < ' ' => $"\\u{(int)c:x4}",
                _ => c.ToString(),
            });
        return b.Append('"').ToString();
    }

    // What JavaScript's String.prototype.trim takes off, which the JS builder trims an aggregate with.
    static bool JsSpace(char c) => c is '\t' or '\n' or '\v' or '\f' or '\r' or ' '
        || (int)c is 0xa0 or 0x1680 or 0x2028 or 0x2029 or 0x202f or 0x205f or 0x3000 or 0xfeff or (>= 0x2000 and <= 0x200a);

    static bool AsciiIdent(string s) =>
        s.Length > 0 && (char.IsAsciiLetter(s[0]) || s[0] == '_') && s.All(c => char.IsAsciiLetterOrDigit(c) || c == '_');

    /// <summary>A select item: a field, or an aggregate spelled as FenecQL spells it -- count(*), sum(total),
    /// avg(f), min(f), max(f) -- answering under that name. Read by hand rather than by a case-blind pattern,
    /// which folds the Kelvin sign onto k where the JS builder's does not.</summary>
    public static (string Text, bool Aggregate) Column(string? name)
    {
        if (name is not null)
        {
            var start = 0;
            var end = name.Length;
            while (start < end && JsSpace(name[start])) start++;
            while (end > start && JsSpace(name[end - 1])) end--;
            var s = name[start..end];
            var open = s.IndexOf('(');
            if (open > 0 && s.EndsWith(')'))
            {
                string fn = s[..open], arg = s[(open + 1)..^1];
                var low = fn.All(char.IsAsciiLetter) ? fn.ToLowerInvariant() : null;
                if (low == "count" && arg is "" or "*") return ("count(*)", true);
                if (low is "sum" or "avg" or "min" or "max" && AsciiIdent(arg)) return ($"{low}({arg})", true);
            }
        }
        return (FieldPath(name), false);
    }

    public static bool Direction(string? dir) => dir?.ToLowerInvariant() switch
    {
        "asc" => true,
        "desc" => false,
        _ => throw Refuse($"order direction must be 'asc' or 'desc': {dir}"),
    };

    // The collations the engine knows. The name is spliced into the text, so it is checked against the list
    // rather than the name pattern.
    public static string? Collation(string? name) => name switch
    {
        null => null,
        "und" or "tr" => name,
        _ => throw Refuse($"unknown collation: {Quote(name)}; there are 'und' and 'tr'"),
    };

    // limit, offset, ef and the rest are literals in FenecQL, never parameters: a whole number JavaScript holds
    // exactly.
    public static long Whole(long n, string what) =>
        n is >= 0 and <= (1L << 53) - 1 ? n : throw Refuse($"{what} must be a non-negative integer: {n}");

    /// <summary>A value as it goes into the parameters: a DateTime or DateTimeOffset as the text JavaScript's
    /// toISOString writes, which the JS builder sends a Date as -- UTC, to the millisecond (an unspecified
    /// DateTime is taken for UTC) -- inside lists and dictionaries too.</summary>
    public static object? Normalize(object? v) => v switch
    {
        null or string or float[] or ReadOnlyMemory<float> or Memory<float> or JsonElement => v,
        DateTime d => (d.Kind == DateTimeKind.Local ? d.ToUniversalTime() : d)
            .ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", CultureInfo.InvariantCulture),
        DateTimeOffset d => d.UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", CultureInfo.InvariantCulture),
        IDictionary dict => Entries(dict).ToDictionary(
            e => Convert.ToString(e.Key, CultureInfo.InvariantCulture)!, e => Normalize(e.Value)),
        IEnumerable list => list.Cast<object?>().Select(Normalize).ToList(),
        _ => v,
    };

    public static Node CondOf(string field, string op, object? value)
    {
        if (!Ops.TryGetValue(op ?? "", out var o)) throw Refuse($"unknown operator `{op}`");
        var f = FieldPath(field);
        return o == "in" ? InCond(f, value) : CmpNode(f, o, value);
    }

    /// <summary>One field's condition: a value is equality, null is null, a dictionary its operators.</summary>
    public static Node FieldCond(string field, object? spec)
    {
        if (spec is null) return new Node("null") { Field = field };
        if (spec is not IDictionary ops) return CmpNode(field, "=", spec);
        var items = new List<Node>();
        foreach (DictionaryEntry e in ops)
        {
            var k = Convert.ToString(e.Key, CultureInfo.InvariantCulture) ?? "";
            if (k == "not")
            {
                items.Add(e.Value is null
                    ? new Node("null") { Field = field, Negated = true }
                    : new Node("not") { Items = [FieldCond(field, e.Value)] });
                continue;
            }
            if (!Ops.TryGetValue(k, out var op)) throw Refuse($"unknown operator `{k}` (field: {field})");
            items.Add(op == "in" ? InCond(field, e.Value) : CmpNode(field, op, e.Value));
        }
        return items.Count switch
        {
            0 => throw Refuse($"empty condition object (field: {field})"),
            1 => items[0],
            _ => new Node("and") { Items = items },
        };
    }

    static Node InCond(string field, object? values)
    {
        if (values is null or string or IDictionary || values is not IEnumerable list)
            throw Refuse($"`in` expects an array (field: {field})");
        var items = list.Cast<object?>().ToList();
        if (items.Count == 0) throw Refuse($"`in` does not accept an empty array (field: {field})");
        return new Node("in") { Field = field, Values = items };
    }

    // `= null` is never true in FenecQL; what is meant is `is null`.
    static Node CmpNode(string field, string op, object? value) => value is not null
        ? new Node("cmp") { Field = field, Op = op, Value = value }
        : op switch
        {
            "=" => new Node("null") { Field = field },
            "!=" => new Node("null") { Field = field, Negated = true },
            _ => throw Refuse($"`{op}` cannot be used with null (field: {field})"),
        };

    // Flattens empty and single-child junctions before rendering: the parentheses depend on the child count,
    // and rendering binds parameters.
    public static Node? Prune(Node c)
    {
        if (c.T is "and" or "or")
        {
            var items = c.Items.Select(Prune).OfType<Node>().ToList();
            return items.Count switch { 0 => null, 1 => items[0], _ => new Node(c.T) { Items = items } };
        }
        if (c.T == "not")
            return Prune(c.Items[0]) is { } item ? new Node("not") { Items = [item] } : null;
        return c;
    }

    public static string Render(Node c, Binder bind, string? parent = null)
    {
        switch (c.T)
        {
            case "and" or "or":
                var s = string.Join($" {c.T} ", c.Items.Select(x => Render(x, bind, c.T)));
                // `and` binds tighter than `or`: one inside the other needs parens.
                return parent is not null && parent != c.T ? $"({s})" : s;
            case "not":
                return $"not ({Render(c.Items[0], bind)})";
            case "null":
                return $"{c.Field} is {(c.Negated ? "not " : "")}null";
            case "in":
                return $"{c.Field} in [{string.Join(", ", c.Values.Select(bind.Bind))}]";
            case "cmp":
                return $"{c.Field} {c.Op} {bind.Bind(c.Value)}";
        }
        var pieces = c.Sql.Split('?');
        var out_ = new StringBuilder();
        for (var i = 0; i < pieces.Length - 1; i++)
        {
            if (i >= c.Values.Count) throw Refuse("raw(): more `?` placeholders than parameters");
            out_.Append(pieces[i]).Append(bind.Bind(c.Values[i]));
        }
        if (pieces.Length - 1 != c.Values.Count) throw Refuse("raw(): too many parameters given");
        return out_.Append(pieces[^1]).ToString();
    }

    /// <summary>A document's fields in order: a dictionary's, or an object's public properties as declared,
    /// named as System.Text.Json names them by default -- <c>[JsonPropertyName]</c>, else the property's name.</summary>
    public static List<KeyValuePair<string, object?>> DocOf(object? doc)
    {
        switch (doc)
        {
            case IDictionary dict:
                return Entries(dict)
                    .Select(e => new KeyValuePair<string, object?>(Convert.ToString(e.Key, CultureInfo.InvariantCulture)!, e.Value))
                    .ToList();
            case null or IEnumerable:
                throw Refuse("expected a document object");
        }
        // A number, a date, a text is no document; a struct of fields is.
        if (Type.GetTypeCode(doc.GetType()) != TypeCode.Object) throw Refuse("expected a document object");
        return doc.GetType().GetProperties(BindingFlags.Public | BindingFlags.Instance)
            .Where(p => p.GetIndexParameters().Length == 0 && p.GetCustomAttribute<JsonIgnoreAttribute>() is null)
            .OrderBy(p => p.MetadataToken)
            .Select(p => new KeyValuePair<string, object?>(
                p.GetCustomAttribute<JsonPropertyNameAttribute>()?.Name ?? p.Name, p.GetValue(doc)))
            .ToList();
    }

    public static string RenderDoc(object? doc, Binder bind, string write)
    {
        var fields = DocOf(doc);
        if (fields.Count == 0) throw Refuse("cannot write an empty document");
        return "{" + string.Join(", ", fields.Select(kv =>
        {
            var name = FieldPath(kv.Key);
            return $"{name}: {(kv.Value is Computed c ? c.Render(name, bind, write) : bind.Bind(kv.Value))}";
        })) + "}";
    }

    static readonly JsonSerializerOptions Relaxed =
        new() { Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping };

    /// <summary>A value as JSON.stringify writes it, for a message to quote.</summary>
    public static string JsJson(object? v)
    {
        try { return JsonSerializer.Serialize(v, Relaxed); }
        catch (NotSupportedException) { return Convert.ToString(v, CultureInfo.InvariantCulture) ?? "null"; }
    }
}

internal sealed class Binder
{
    public List<object?> Params { get; } = [];

    public string Bind(object? v)
    {
        Params.Add(Builder.Normalize(v));
        return $"${Params.Count}";
    }
}

/// <summary>
/// A query over one collection, made by <see cref="FenecClient.From"/> or <see cref="From(string)"/>. Immutable:
/// each call hands back a new one, so a base query can be kept and branched from, from several threads too. A
/// step that is refused throws <see cref="FenecQueryException"/> as it is called, and so does a statement whose
/// clauses do not go together, when it is made.
/// </summary>
public sealed class Query
{
    sealed record VectorClause(string Field, object? Vector, long? N, bool Exact);

    sealed record Level(string Collection, string On, string? Parent, IReadOnlyList<string>? Project,
        IReadOnlyList<Node> Cond, bool Required, IReadOnlyList<(string Field, bool Asc, string? Collate)> Order,
        long? Limit, long Offset);

    // A highlight when Words is null, a snippet otherwise.
    sealed record Mark(string Field, long? Words, string? Ellipsis, string? Pre, string? Post)
    {
        public string Kind => Words is null ? "highlight" : "snippet";
    }

    sealed record FacetClause(string Field, long? Top);

    sealed record State(string Collection, FenecClient? Client)
    {
        public IReadOnlyList<string>? Project { get; init; }
        public bool Aggregate { get; init; }
        public string? Group { get; init; }
        public IReadOnlyList<Node> Cond { get; init; } = [];
        public VectorClause? Near { get; init; }
        public (string Field, object? Query)? Match { get; init; }
        public VectorClause? Rerank { get; init; }
        public (long? K, long? Candidates)? Fuse { get; init; }
        public IReadOnlyList<(string Field, bool Asc, string? Collate)> Order { get; init; } = [];
        public long? Limit { get; init; }
        public long Offset { get; init; }
        public string Require { get; init; } = "";
        public bool Count { get; init; }
        public IReadOnlyList<Level> Lookups { get; init; } = [];
        public IReadOnlyList<Mark> Marks { get; init; } = [];
        public IReadOnlyList<FacetClause> Facets { get; init; } = [];
    }

    readonly State _s;

    Query(State s) => _s = s;

    internal Query(string collection, FenecClient? client) =>
        _s = new State(Builder.Ident(collection, "collection"), client);

    /// <summary>A query bound to no client: for its text alone, <see cref="ToFenecQL"/>.</summary>
    public static Query From(string collection) => new(collection, null);

    /// <summary>The collection the query is over.</summary>
    public string Collection => _s.Collection;

    /// <summary><c>select a, b</c>; none, or <c>"*"</c>, is every field. Aggregates go in the same list as
    /// FenecQL spells them and answer under that name: <c>Select("status", "count(*)", "sum(total)").Group("status")</c>.</summary>
    public Query Select(params string[] columns)
    {
        if (columns.Length == 0 || columns.Contains("*")) return new(_s with { Project = null, Aggregate = false });
        var cs = columns.Select(Builder.Column).ToList();
        return new(_s with { Project = cs.Select(c => c.Text).ToList(), Aggregate = cs.Any(c => c.Aggregate) });
    }

    /// <summary><c>group field</c>: a row per value, for a select list that aggregates.</summary>
    public Query Group(string field) => new(_s with { Group = Builder.Ident(field) });

    /// <summary><c>field op value</c>, joined to the conditions before with <c>and</c>. The op is a symbol or its
    /// word: <c>=</c>, <c>!=</c>, <c>&lt;</c>, <c>&lt;=</c>, <c>&gt;</c>, <c>&gt;=</c>, <c>~</c>, <c>has</c>,
    /// <c>in</c> (a list), or <c>eq</c>, <c>ne</c>, <c>lt</c>, <c>gte</c>, <c>like</c>, <c>contains</c> ... A null
    /// with <c>=</c> or <c>!=</c> is <c>is null</c> and <c>is not null</c>.</summary>
    public Query Where(string field, string op, object? value) => And(Builder.CondOf(field, op, value));

    /// <summary>The field held to a spec: a value is equality, null is null, a dictionary its operators.</summary>
    public Query Where(string field, object? spec) => And(Builder.FieldCond(Builder.FieldPath(field), spec));

    /// <summary>A condition <see cref="Cond"/> made, or a dictionary of fields.</summary>
    public Query Where(Cond cond) => And(cond.Node);

    /// <summary>Everything conditioned so far, or <c>field op value</c>.</summary>
    public Query OrWhere(string field, string op, object? value) => Or(Builder.CondOf(field, op, value));

    /// <summary>Everything conditioned so far, or the field held to a spec.</summary>
    public Query OrWhere(string field, object? spec) => Or(Builder.FieldCond(Builder.FieldPath(field), spec));

    /// <summary>Everything conditioned so far, or the condition.</summary>
    public Query OrWhere(Cond cond) => Or(cond.Node);

    Query And(Node c) => new(_s with { Cond = [.. _s.Cond, c] });

    Query Or(Node right) => _s.Cond.Count == 0
        ? new(_s with { Cond = [right] })
        : new(_s with { Cond = [new Node("or") { Items = [new Node("and") { Items = [.. _s.Cond] }, right] }] });

    /// <summary><c>near field $n [ef N] [exact]</c>.</summary>
    public Query Near(string field, object? vector, long? ef = null, bool exact = false) =>
        new(_s with { Near = new(Builder.Ident(field), vector, ef is { } n ? Builder.Whole(n, "ef") : null, exact) });

    /// <summary><c>match field $n</c>: BM25 over a <c>@text</c> index.</summary>
    public Query Match(string field, string query) => new(_s with { Match = (Builder.Ident(field), query) });

    /// <summary><c>fuse [k N] [candidates N]</c>: with both Match and Near, ranks by both, a document scoring
    /// <c>1 / (k + rank)</c> from each list it is on.</summary>
    public Query Fuse(long? k = null, long? candidates = null) => new(_s with
    {
        Fuse = (k is { } a ? Builder.Whole(a, "k") : null,
            candidates is { } c ? Builder.Whole(c, "candidates") : null),
    });

    /// <summary><c>rerank field $n [candidates N]</c>: reorders what Match found by exact distance, the vectors
    /// read out of the store.</summary>
    public Query Rerank(string field, object? vector, long? candidates = null) => new(_s with
    {
        Rerank = new(Builder.Ident(field), vector,
            candidates is { } c ? Builder.Whole(c, "candidates") : null, false),
    });

    /// <summary>
    /// <c>highlight(field)</c> in the select list: where the terms Match found stand in the field's text --
    /// <c>[start, end]</c> pairs of UTF-16 offsets, a .NET string's own -- or, given <paramref name="pre"/> and
    /// <paramref name="post"/>, the text with each mark between them. The text is not escaped: a page that renders
    /// it as HTML builds it from the offsets, or escapes it first. Answers under <c>highlight(field)</c>, after the
    /// fields Select named.
    /// <code>db.From("docs").Select("title").Highlight("body", "&lt;mark&gt;", "&lt;/mark&gt;").Match("body", text)</code>
    /// </summary>
    public Query Highlight(string field, string? pre = null, string? post = null) => Highlighted(field, pre, post);

    /// <summary><c>snippet(field, words)</c>: the window of <paramref name="words"/> words around the densest
    /// marks, <c>{"text": ..., "marks": [[s, e], ...]}</c> -- or the marked text, given <paramref name="pre"/> and
    /// <paramref name="post"/> -- with <paramref name="ellipsis"/> where it leaves text out. Answers under
    /// <c>snippet(field)</c>.</summary>
    public Query Snippet(string field, long words, string? ellipsis = null, string? pre = null, string? post = null) =>
        Snipped(field, words, ellipsis, pre, post);

    // The steps' checks over values of any type, as the JS builder's take them: a C# caller cannot hand a tag
    // that is not text, but the golden file's chains do, and the refusal has to be the JS builder's word for word.
    internal Query Highlighted(string field, object? pre, object? post)
    {
        var f = Builder.Ident(field);
        var (p, q) = Tags(pre, post, "highlight");
        return WithMark(new Mark(f, null, null, p, q));
    }

    internal Query Snipped(string field, long words, object? ellipsis, object? pre, object? post)
    {
        var f = Builder.Ident(field);
        var n = Builder.Whole(words, "snippet words");
        var (p, q) = Tags(pre, post, "snippet");
        if (n == 0) throw Builder.Refuse("snippet shows at least one word");
        var e = ellipsis is null ? null : Text(ellipsis, "snippet ellipsis");
        return WithMark(new Mark(f, n, e, p, q));
    }

    static (string?, string?) Tags(object? pre, object? post, string what)
    {
        if (pre is null && post is null) return (null, null);
        if (pre is null || post is null) throw Builder.Refuse($"{what} takes both pre and post, or neither");
        return (Text(pre, $"{what} pre"), Text(post, $"{what} post"));
    }

    static string Text(object v, string what) => v as string
        ?? throw Builder.Refuse($"{what} must be text: {System.Text.Json.JsonSerializer.Serialize(v)}");

    // Each answers under its label, and a row holds a name once.
    Query WithMark(Mark m) => _s.Marks.Any(x => x.Kind == m.Kind && x.Field == m.Field)
        ? throw Builder.Refuse($"{m.Kind}({m.Field}) is asked twice")
        : new(_s with { Marks = [.. _s.Marks, m] });

    /// <summary><c>facet field [top N]</c>: each value the field -- or a path into a json field,
    /// <c>meta.lang</c> -- holds over every row the query matches, not only the page, and how many rows hold it,
    /// most first; <paramref name="top"/> keeps the commonest. A list counts once a row for each value. The counts
    /// come back beside the rows: <see cref="AnswerAsync"/>'s <see cref="Answer.Facets"/>.
    /// <code>db.From("products").Match("title", "phone").Where("price", "&lt;", 500).Facet("brand", top: 10).Facet("color").Limit(20)</code>
    /// </summary>
    public Query Facet(string field, long? top = null)
    {
        var f = new FacetClause(Builder.FieldPath(field), top is { } t ? Builder.Whole(t, "facet top") : null);
        if (f.Top == 0) throw Builder.Refuse($"facet {f.Field} top 0 answers nothing");
        if (_s.Facets.Any(g => g.Field == f.Field)) throw Builder.Refuse($"facet {f.Field} is asked twice");
        return new(_s with { Facets = [.. _s.Facets, f] });
    }

    /// <summary>
    /// <c>lookup name on child [= parent] ...</c>: each row's children, attached to it. <paramref name="on"/>
    /// names the child's field, <paramref name="parentKey"/> the parent's (<c>id</c> unless given); the rest
    /// binds to the looked-up collection, and <paramref name="limit"/> counts children per parent.
    /// <paramref name="required"/> drops a parent no child matches. Called again, it chains onto the collection
    /// the call before named.
    /// </summary>
    public Query Lookup(string name, string? on = null, string? parentKey = null, IEnumerable<string>? select = null,
        Cond? where = null, bool required = false, IEnumerable<SortKey>? order = null, long? limit = null,
        long? offset = null)
    {
        if (string.IsNullOrEmpty(on)) throw Builder.Refuse("lookup needs `on`: the child field holding the key");
        var collection = Builder.Ident(name, "collection");
        var child = Builder.Ident(on);
        var parent = parentKey is null ? null : Builder.Ident(parentKey);
        var cols = select?.ToList();
        var project = cols is null || cols.Contains("*") ? null : cols.Select(Builder.FieldPath).ToList();
        var keys = (order ?? []).Select(k =>
            (Builder.FieldPath(k.Field), Builder.Direction(k.Direction), Builder.Collation(k.Collate))).ToList();
        var level = new Level(collection, child, parent, project, where is null ? [] : [where.Node], required, keys,
            limit is { } l ? Builder.Whole(l, "limit") : null, offset is { } o ? Builder.Whole(o, "offset") : 0);
        return new(_s with { Lookups = [.. _s.Lookups, level] });
    }

    /// <summary><c>order field asc|desc</c>: each call adds a key. <paramref name="collate"/> <c>"tr"</c> puts
    /// text in Turkish order, <c>"und"</c> in Unicode's root order.</summary>
    public Query Order(string field, string direction = "asc", string? collate = null) => new(_s with
    {
        // Over groups a key may be an aggregate of the list, by its name.
        Order = [.. _s.Order, (Builder.Column(field).Text, Builder.Direction(direction), Builder.Collation(collate))],
    });

    /// <summary>How many rows come back.</summary>
    public Query Limit(long n) => new(_s with { Limit = Builder.Whole(n, "limit") });

    /// <summary>How many rows are passed over first.</summary>
    public Query Offset(long n) => new(_s with { Offset = Builder.Whole(n, "offset") });

    /// <summary>
    /// <c>require n</c> on a read: the rows it answers, after <see cref="Limit"/>, must number n, or it is refused
    /// (412, unmet) and the batch it is in put back, as a write's <c>require</c> is -- a checkout's guard on a read.
    /// </summary>
    public Query Require(long n) => new(_s with { Require = RequireClause(n) });

    /// <summary>The statement and its parameters, as they would be sent.</summary>
    public (string Text, IReadOnlyList<object?> Parameters) ToFenecQL()
    {
        var s = _s;
        // The engine refuses each of these too; failing here sends nothing.
        if (s.Group is not null && !s.Aggregate)
            throw Builder.Refuse($"group {s.Group} needs an aggregate in select: 'count(*)'");
        if (s.Aggregate)
        {
            var clash = s.Near is not null ? "near" : s.Match is not null ? "match"
                : s.Lookups.Count > 0 ? "lookup" : s.Count ? "count" : null;
            if (clash is not null) throw Builder.Refuse($"aggregates cannot be combined with {clash}");
            if (s.Group is null && (s.Order.Count > 0 || s.Limit is not null || s.Offset > 0))
                throw Builder.Refuse("aggregates answer one row; group makes a row per value");
            if (s.Group is null && s.Require.Length > 0)
                throw Builder.Refuse("require counts the rows a query answers, and an aggregate answers one");
        }
        if (s.Marks.Count > 0)
        {
            var what = s.Marks[0].Kind;
            if (s.Match is null) throw Builder.Refuse($"{what} needs match: it marks the terms match found");
            if (s.Aggregate) throw Builder.Refuse($"{what} marks a row's text; aggregates answer groups");
        }
        if (s.Facets.Count > 0)
        {
            if (s.Near is not null)
                throw Builder.Refuse("facet counts the rows a filter or match selects, and near ranks every row: ask the facets without near");
            if (s.Aggregate) throw Builder.Refuse("facet cannot be combined with aggregates: group counts by value");
        }
        if (s.Rerank is not null && s.Match is null)
            throw Builder.Refuse("rerank needs match: it reorders what match found");
        if (s.Match is not null && s.Near is not null && s.Fuse is null)
            throw Builder.Refuse("match and near cannot be combined: both order the result; fuse() ranks by both");
        if (s.Fuse is not null && (s.Match is null || s.Near is null))
            throw Builder.Refuse("fuse combines match and near: the query needs both");
        if (s.Fuse is not null && s.Rerank is not null)
            throw Builder.Refuse("fuse and rerank are two ways to use a vector with match: pick one");
        if (s.Lookups.Count > 0)
        {
            var clash = s.Near is not null ? "near" : s.Match is not null ? "match" : s.Rerank is not null ? "rerank" : null;
            if (clash is not null) throw Builder.Refuse($"lookup cannot be combined with {clash}");
            if (s.Count && !s.Lookups[0].Required)
                throw Builder.Refuse("count cannot be used with lookup unless it is required: there is nothing to attach children to");
            if (s.Lookups.Count > Builder.MaxLookupDepth)
                throw Builder.Refuse($"lookup chained too deep: at most {Builder.MaxLookupDepth} levels");
            var seen = new List<string> { s.Collection };
            foreach (var l in s.Lookups)
            {
                if (seen.Contains(l.Collection))
                    throw Builder.Refuse($"{l.Collection} cannot look itself up: both sides would answer to the same name");
                seen.Add(l.Collection);
            }
        }
        if (s.Count && (ExtraClause() ?? (s.Require.Length > 0 ? "require" : null)) is { } extra)
            throw Builder.Refuse($"count cannot be used with `{extra}`");

        var bind = new Binder();
        var sql = new StringBuilder($"get {s.Collection}");
        // The marks after the fields Select named, or after every field. Their tags are bound before the
        // where's values, as they come first in the text.
        var items = s.Marks.Select(m =>
        {
            var item = new StringBuilder($"{m.Kind}({m.Field}");
            if (m.Words is { } words) item.Append($", {words}");
            // A snippet's tags come after its ellipsis, so tags alone take the empty one.
            if (m.Ellipsis is not null || (m.Words is not null && m.Pre is not null))
                item.Append(", ").Append(bind.Bind(m.Ellipsis ?? ""));
            if (m.Pre is not null) item.Append(", ").Append(bind.Bind(m.Pre)).Append(", ").Append(bind.Bind(m.Post));
            return item.Append(')').ToString();
        }).ToList();
        if (s.Project is not null || items.Count > 0)
            sql.Append(" select ").Append(string.Join(", ", [.. s.Project ?? ["*"], .. items]));
        if (WhereOf(s.Cond, bind) is { } where) sql.Append(" where ").Append(where);
        if (s.Group is not null) sql.Append(" group ").Append(s.Group);
        if (s.Near is { } near)
        {
            sql.Append($" near {near.Field} {bind.Bind(near.Vector)}");
            if (near.N is { } ef) sql.Append($" ef {ef}");
            if (near.Exact) sql.Append(" exact");
        }
        if (s.Match is { } match) sql.Append($" match {match.Field} {bind.Bind(match.Query)}");
        if (s.Rerank is { } rerank)
        {
            sql.Append($" rerank {rerank.Field} {bind.Bind(rerank.Vector)}");
            if (rerank.N is { } c) sql.Append($" candidates {c}");
        }
        if (s.Fuse is { } fuse)
        {
            sql.Append(" fuse");
            if (fuse.K is { } k) sql.Append($" k {k}");
            if (fuse.Candidates is { } c) sql.Append($" candidates {c}");
        }
        AppendOrder(sql, s.Order);
        if (s.Limit is { } limit) sql.Append($" limit {limit}");
        if (s.Offset > 0) sql.Append($" offset {s.Offset}");
        // Before a lookup, whose clauses are the children's.
        sql.Append(s.Require);
        if (s.Count) sql.Append(" count");
        if (s.Facets.Count > 0)
            sql.Append(" facet ").Append(string.Join(", ",
                s.Facets.Select(f => f.Top is { } top ? $"{f.Field} top {top}" : f.Field)));
        // Terminal, so every clause after it is the child's -- and last, so its parameters come after the
        // parent's.
        foreach (var l in s.Lookups)
        {
            sql.Append($" lookup {l.Collection} on {l.On}");
            if (l.Parent is not null) sql.Append($" = {l.Parent}");
            if (l.Required) sql.Append(" required");
            if (l.Project is not null) sql.Append(" select ").Append(string.Join(", ", l.Project));
            if (WhereOf(l.Cond, bind) is { } w) sql.Append(" where ").Append(w);
            AppendOrder(sql, l.Order);
            if (l.Limit is { } n) sql.Append($" limit {n}");
            if (l.Offset > 0) sql.Append($" offset {l.Offset}");
        }
        return (sql.ToString(), bind.Params);
    }

    static void AppendOrder(StringBuilder sql, IReadOnlyList<(string Field, bool Asc, string? Collate)> keys)
    {
        for (var i = 0; i < keys.Count; i++)
        {
            var (field, asc, collate) = keys[i];
            sql.Append(i == 0 ? " order " : ", ").Append(field);
            if (collate is not null) sql.Append(" collate ").Append(collate);
            sql.Append(asc ? " asc" : " desc");
        }
    }

    static string? WhereOf(IReadOnlyList<Node> cond, Binder bind) =>
        Builder.Prune(new Node("and") { Items = [.. cond] }) is { } root ? Builder.Render(root, bind) : null;

    string? ExtraClause() =>
        _s.Near is not null ? "near"
        : _s.Match is not null ? "match"
        : _s.Rerank is not null ? "rerank"
        : _s.Order.Count > 0 ? "order"
        : _s.Limit is not null ? "limit"
        : _s.Offset > 0 ? "offset"
        : _s.Project is not null ? "select"
        : null;

    // Near, Order, Limit mean something only to a read; dropped from a write, Limit(1).DeleteAsync() would
    // delete every row.
    void AssertPlain(string verb)
    {
        if (_s.Require.Length > 0)
            throw Builder.Refuse($"{verb} takes require as its option: {verb}(..., {{ require: n }})");
        if (ExtraClause() is { } extra) throw Builder.Refuse($"{verb} cannot be used with `{extra}`");
        if (_s.Lookups.Count > 0) throw Builder.Refuse($"{verb} cannot be used with `lookup`");
        if (_s.Facets.Count > 0) throw Builder.Refuse($"{verb} cannot be used with `facet`");
        if (verb == "insert" && _s.Cond.Count > 0) throw Builder.Refuse("insert cannot be used with `where`");
    }

    // An update or delete of every row is too easy to do by accident and cannot be undone: it has to be asked
    // for, with all.
    string RequireFilter(string verb, bool all, Binder bind)
    {
        if (WhereOf(_s.Cond, bind) is { } where) return $" where {where}";
        if (all) return "";
        throw Builder.Refuse($"an unfiltered {verb} covers the whole collection; if you mean it, {verb}({{ all: true }})");
    }

    // " require n" for a write's require: the rows it must write, a whole number from 0 written into the text
    // -- not a parameter, as limit is not, so a statement keeps its shape.
    static string RequireClause(long? n) => n switch
    {
        null => "",
        < 0 => throw Builder.Refuse($"require takes a count of rows, a whole number from 0 (got {n})"),
        _ => $" require {n}",
    };

    static List<object?> DocsOf(object? docs) =>
        docs is IEnumerable list and not IDictionary and not string ? list.Cast<object?>().ToList() : [docs];

    /// <summary>The <c>put</c> of a document -- a dictionary, or an object's public properties -- or a list of
    /// them, not sent. With <paramref name="ifAbsent"/>, <c>put ... if absent</c>: a document whose id or
    /// <c>@unique</c> value a row holds is passed over, and not counted. With <paramref name="require"/>, on any
    /// write, <c>... require n</c>: unless it wrote exactly n rows it is refused (412, <c>unmet</c>) and its
    /// batch put back.</summary>
    public (string Text, IReadOnlyList<object?> Parameters) ToInsert(object docs, bool ifAbsent = false, long? require = null)
    {
        AssertPlain("insert");
        var list = DocsOf(docs);
        if (list.Count == 0) throw Builder.Refuse("cannot write an empty document list");
        var bind = new Binder();
        var body = string.Join(", ", list.Select(d => Builder.RenderDoc(d, bind, "insert")));
        var absent = ifAbsent ? " if absent" : "";
        return ($"put {_s.Collection} {(list.Count == 1 ? body : $"[{body}]")}{absent}{RequireClause(require)}", bind.Params);
    }

    /// <summary>The <c>set</c> of the rows the filter names, not sent; with no filter it is refused unless
    /// <paramref name="all"/>.</summary>
    public (string Text, IReadOnlyList<object?> Parameters) ToUpdate(object patch, bool all = false, long? require = null)
    {
        AssertPlain("update");
        var bind = new Binder();
        var body = Builder.RenderDoc(patch, bind, "update");
        var where = RequireFilter("update", all, bind);
        return ($"set {_s.Collection} {body}{where}{RequireClause(require)}", bind.Params);
    }

    /// <summary>The <c>del</c> of the rows the filter names, not sent; with no filter it is refused unless
    /// <paramref name="all"/>.</summary>
    public (string Text, IReadOnlyList<object?> Parameters) ToDelete(bool all = false, long? require = null)
    {
        AssertPlain("delete");
        var bind = new Binder();
        var where = RequireFilter("delete", all, bind);
        return ($"del {_s.Collection}{where}{RequireClause(require)}", bind.Params);
    }

    FenecClient Client => _s.Client ?? throw Builder.Refuse(
        "query is not bound to a connection: use db.From(...) (ToFenecQL() if you only want the text)");

    async Task<byte[]> BodyAsync(CancellationToken ct)
    {
        var (text, ps) = ToFenecQL();
        var (body, _) = await Client.RunAsync(text, ps, ct).ConfigureAwait(false);
        return body;
    }

    /// <summary>Runs the query and hands back its rows.</summary>
    public async Task<IReadOnlyList<JsonElement>> RowsAsync(CancellationToken cancellationToken = default) =>
        FenecClient.RowsOf(await BodyAsync(cancellationToken).ConfigureAwait(false));

    /// <summary>Runs the query and hands back its rows and what its <see cref="Facet"/> clauses counted beside
    /// them.</summary>
    public async Task<Answer> AnswerAsync(CancellationToken cancellationToken = default) =>
        FenecClient.AnswerOf(await BodyAsync(cancellationToken).ConfigureAwait(false));

    /// <summary>Runs the query and maps its rows to <typeparamref name="T"/> by property name, as
    /// <see cref="FenecClient.QueryAsync{T}"/> does.</summary>
    public async Task<IReadOnlyList<T>> RowsAsync<T>(CancellationToken cancellationToken = default) =>
        (await RowsAsync(cancellationToken).ConfigureAwait(false))
        .Select(r => r.Deserialize<T>(FenecClient.ByName)!).ToList();

    /// <summary>The first row of the query with <c>limit 1</c>, or null.</summary>
    public async Task<JsonElement?> FirstAsync(CancellationToken cancellationToken = default)
    {
        var rows = await Limit(1).RowsAsync(cancellationToken).ConfigureAwait(false);
        return rows.Count > 0 ? rows[0] : null;
    }

    /// <summary>The first row of the query with <c>limit 1</c> as a <typeparamref name="T"/>, or its default.</summary>
    public async Task<T?> FirstAsync<T>(CancellationToken cancellationToken = default)
    {
        var rows = await Limit(1).RowsAsync<T>(cancellationToken).ConfigureAwait(false);
        return rows.Count > 0 ? rows[0] : default;
    }

    /// <summary>How many rows match: <c>get ... count</c>, no row decoded.</summary>
    public async Task<long> CountAsync(CancellationToken cancellationToken = default)
    {
        var rows = await new Query(_s with { Count = true }).RowsAsync(cancellationToken).ConfigureAwait(false);
        return rows.Count > 0 && rows[0].TryGetProperty("count", out var n) ? n.GetInt64() : 0;
    }

    /// <summary>The path the query took, a line a step; the query runs to tell.</summary>
    public async Task<IReadOnlyList<string>> ExplainAsync(CancellationToken cancellationToken = default)
    {
        var (text, ps) = ToFenecQL();
        var (body, _) = await Client.RunAsync($"explain {text}", ps, cancellationToken).ConfigureAwait(false);
        return FenecClient.RowsOf(body)
            .Select(r => r.TryGetProperty("plan", out var p) ? p.GetString() ?? "" : "").ToList();
    }

    async Task<ExecResult> ExecAsync((string Text, IReadOnlyList<object?> Parameters) statement, CancellationToken ct) =>
        await Client.ExecAsync(statement.Text, statement.Parameters, ct).ConfigureAwait(false);

    /// <summary>Puts a document -- a dictionary, or an object's public properties -- or a list of them; the
    /// result's <c>Affected</c> is how many were written, which with <paramref name="ifAbsent"/> leaves out those
    /// whose id or <c>@unique</c> value was held: a lock taken answers 1, one held 0. None is no request. With
    /// <paramref name="require"/> (on an update and a delete too) a write that did not write exactly that many rows
    /// is refused, 412 (<c>unmet</c>), and its batch put back.</summary>
    public Task<ExecResult> InsertAsync(object docs, bool ifAbsent = false, long? require = null,
        CancellationToken cancellationToken = default) =>
        DocsOf(docs).Count == 0
            ? Task.FromResult(new ExecResult(0, null, 0, false))
            : ExecAsync(ToInsert(docs, ifAbsent, require), cancellationToken);

    /// <summary>Sets the patch's fields on the rows the filter names; with no filter it is refused unless
    /// <paramref name="all"/>.</summary>
    public Task<ExecResult> UpdateAsync(object patch, bool all = false, long? require = null,
        CancellationToken cancellationToken = default) =>
        ExecAsync(ToUpdate(patch, all, require), cancellationToken);

    /// <summary>Deletes the rows the filter names; with no filter it is refused unless <paramref name="all"/>.</summary>
    public Task<ExecResult> DeleteAsync(bool all = false, long? require = null,
        CancellationToken cancellationToken = default) =>
        ExecAsync(ToDelete(all, require), cancellationToken);
}
