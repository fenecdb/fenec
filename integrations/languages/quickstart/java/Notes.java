import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.util.ArrayList;
import java.util.regex.Pattern;

public class Notes {
    static final HttpClient HTTP = HttpClient.newHttpClient();

    // One FenecQL statement, as the JSON {"query": ..., "params": [...]}.
    static HttpResponse<String> query(String json) throws Exception {
        var req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:8080/query"))
            .header("Content-Type", "application/json")
            .POST(HttpRequest.BodyPublishers.ofString(json))
            .build();
        return HTTP.send(req, HttpResponse.BodyHandlers.ofString());
    }

    // The standard library has no JSON: every "title" of an answer, or its "error".
    static String find(String key, String body) {
        var found = new ArrayList<String>();
        var m = Pattern.compile("\"" + key + "\":\"([^\"]*)\"").matcher(body);
        while (m.find()) found.add(m.group(1));
        return String.join(", ", found);
    }

    public static void main(String[] args) throws Exception {
        query("""
            {"query": "put notes {title: $1, year: $2, kind: $3}", "params": ["Sand and stars", 2023, "book"]}""");

        var rows = query("""
            {"query": "get notes where year >= $1 order year desc", "params": [2020]}""");
        System.out.println("from 2020: " + find("title", rows.body()));

        var hits = query("""
            {"query": "get notes select title match title $1 fuzzy", "params": ["oasys"]}""");
        System.out.println("search: " + find("title", hits.body()));

        var refused = query("""
            {"query": "get nowhere"}""");
        System.out.println("refused: " + refused.statusCode() + " " + find("error", refused.body()));
    }
}
