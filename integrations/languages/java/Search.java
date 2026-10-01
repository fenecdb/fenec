// Java with java.net.http alone (11 and newer), run as a source file:
// java Search.java. The standard library has no JSON, so the statements are
// written as text and the answer is read with a pattern; a project reads it
// with Jackson or Gson. Run by ../run-tests.sh.
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.util.List;
import java.util.regex.Pattern;

public class Search {
    static final String URL = System.getenv().getOrDefault("FENEC_URL", "http://127.0.0.1:8080");
    static final String TOKEN = System.getenv("FENEC_TOKEN");
    static final HttpClient HTTP = HttpClient.newHttpClient();

    // One FenecQL statement, its body the JSON {"query": ..., "params": [...]}.
    static HttpResponse<String> query(String json) throws Exception {
        var req = HttpRequest.newBuilder(URI.create(URL + "/query"))
            .header("Content-Type", "application/json")
            .POST(HttpRequest.BodyPublishers.ofString(json));
        if (TOKEN != null) req.header("Authorization", "Bearer " + TOKEN);
        return HTTP.send(req.build(), HttpResponse.BodyHandlers.ofString());
    }

    static String ok(HttpResponse<String> res) {
        if (res.statusCode() >= 300) throw new IllegalStateException(res.statusCode() + ": " + res.body());
        return res.body();
    }

    public static void main(String[] args) throws Exception {
        ok(query("""
            {"query": "create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))"}"""));
        ok(query("""
            {"query": "put docs {title: $1, embed: $2}", "params": ["Night at the oasis", [0.1, 0.2, 0.3]]}"""));
        ok(query("""
            {"query": "put docs {title: $1, embed: $2}", "params": ["Dunes", [0.9, 0.1, 0.0]]}"""));

        String rows = ok(query("""
            {"query": "get docs select title near embed $1 limit 5", "params": [[0.1, 0.2, 0.3]]}"""));
        var titles = Pattern.compile("\"title\":\"([^\"]*)\"").matcher(rows).results()
            .map(m -> m.group(1)).toList();
        if (!titles.equals(List.of("Night at the oasis", "Dunes")))
            throw new AssertionError("near answered " + rows);

        if (query("{\"query\": \"get nowhere\"}").statusCode() != 404)
            throw new AssertionError("a missing collection was answered");

        System.out.println("java: ok");
    }
}
