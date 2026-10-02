using System.Collections;
using System.Globalization;
using System.Text.Json;

namespace FenecDb;

// {"query": ..., "params": [...]}, one line: a batch's lines are its statements.
static class Params
{
    public static void WriteBody(Stream to, string query, IReadOnlyList<object?>? parameters)
    {
        using var w = new Utf8JsonWriter(to);
        w.WriteStartObject();
        w.WriteString("query", query);
        w.WriteStartArray("params");
        foreach (var p in parameters ?? [])
            Write(w, p);
        w.WriteEndArray();
        w.WriteEndObject();
    }

    // A float, alone or in a vector, goes as text that reads back as it
    // exactly; the rest as System.Text.Json writes it -- a list as an array,
    // a dictionary or an object as a json field's value.
    static void Write(Utf8JsonWriter w, object? v)
    {
        switch (v)
        {
            case null: w.WriteNullValue(); break;
            case string s: w.WriteStringValue(s); break;
            case bool b: w.WriteBooleanValue(b); break;
            case float f: w.WriteRawValue(F32(f), skipInputValidation: true); break;
            case double d:
                if (!double.IsFinite(d)) throw new ArgumentException($"{d} is no number JSON can carry");
                w.WriteNumberValue(d);
                break;
            case int or long or short or sbyte or byte or ushort or uint or ulong or decimal:
                w.WriteRawValue(Convert.ToString(v, CultureInfo.InvariantCulture)!, skipInputValidation: true);
                break;
            case float[] a: Vector(w, a); break;
            case ReadOnlyMemory<float> m: Vector(w, m.Span); break;
            case Memory<float> m: Vector(w, m.Span); break;
            case JsonElement e: e.WriteTo(w); break;
            case IDictionary dict:
                w.WriteStartObject();
                foreach (DictionaryEntry kv in dict)
                {
                    w.WritePropertyName(Convert.ToString(kv.Key, CultureInfo.InvariantCulture)!);
                    Write(w, kv.Value);
                }
                w.WriteEndObject();
                break;
            case IEnumerable list:
                w.WriteStartArray();
                foreach (var x in list) Write(w, x);
                w.WriteEndArray();
                break;
            default: JsonSerializer.Serialize(w, v, v.GetType()); break;
        }
    }

    static void Vector(Utf8JsonWriter w, ReadOnlySpan<float> v)
    {
        w.WriteStartArray();
        foreach (var f in v) w.WriteRawValue(F32(f), skipInputValidation: true);
        w.WriteEndArray();
    }

    // The shortest decimal that reads back as f. The server reads a number as
    // a double and rounds that to a float, and one float -- 7.038531e-26 --
    // has a shortest text whose double lies exactly between it and the next
    // float and rounds away: that one goes as its double's text, which
    // rounds back to it.
    internal static string F32(float f)
    {
        if (!float.IsFinite(f)) throw new ArgumentException($"{f} is no number JSON can carry");
        var s = f.ToString("R", CultureInfo.InvariantCulture);
        if ((float)double.Parse(s, CultureInfo.InvariantCulture) != f)
            s = ((double)f).ToString("R", CultureInfo.InvariantCulture);
        return s;
    }
}
