// pgjdbc and pgvector-java against fenec-pg. Each check prints its name; a
// failure is counted and the run exits non-zero, as a test runner would.
import com.pgvector.PGvector;
import java.sql.*;

public class Drivers {
    static int failed = 0;

    interface Body { void run() throws Exception; }

    static void check(String name, Body body) {
        try { body.run(); System.out.println("ok   " + name); }
        catch (Throwable e) { failed++; System.out.println("FAIL " + name + ": " + e); }
    }

    static void expect(boolean cond, String what) { if (!cond) throw new AssertionError(what); }

    public static void main(String[] args) throws Exception {
        String url = System.getenv("FENEC_PG_JDBC");
        Connection[] c = new Connection[1];
        check("connects", () -> c[0] = DriverManager.getConnection(url));
        Connection conn = c[0];
        check("creates a collection", () -> conn.createStatement().execute(
            "create collection if not exists java_docs (title text, year int @hash, embed vector<3> @hnsw(cosine))"));
        check("writes with typed parameters", () -> {
            try (PreparedStatement p = conn.prepareStatement("put java_docs {title: ?, year: ?, embed: [0.1, 0.2, 0.3]}")) {
                p.setString(1, "Night at the oasis"); p.setLong(2, 2024);
                expect(p.executeUpdate() == 1, "one row written");
            }
        });
        check("reads typed rows", () -> {
            try (PreparedStatement p = conn.prepareStatement("get java_docs select title, year where year >= ?")) {
                p.setLong(1, 2020);
                ResultSet r = p.executeQuery();
                expect(r.next(), "a row");
                expect(r.getString(1).equals("Night at the oasis") && r.getLong(2) == 2024, "its values");
            }
        });
        check("commits and rolls back", () -> {
            conn.setAutoCommit(false);
            conn.createStatement().executeUpdate("put java_docs {title: 'kept', year: 2025, embed: [0.3, 0.2, 0.1]}");
            conn.commit();
            conn.createStatement().executeUpdate("put java_docs {title: 'dropped', year: 2025, embed: [0.2, 0.2, 0.2]}");
            conn.rollback();
            conn.setAutoCommit(true);
            ResultSet r = conn.createStatement().executeQuery("get java_docs where title = 'dropped' count");
            expect(r.next() && r.getLong(1) == 0, "the rolled back row is gone");
        });
        check("runs a batch", () -> {
            try (PreparedStatement p = conn.prepareStatement("put java_docs {title: ?, year: 2026, embed: [0.5, 0.5, 0.5]}")) {
                for (int i = 0; i < 3; i++) { p.setString(1, "batch " + i); p.addBatch(); }
                expect(p.executeBatch().length == 3, "three statements");
            }
        });
        check("writes a PGvector and searches near one", () -> {
            PGvector.addVectorType(conn);
            try (PreparedStatement p = conn.prepareStatement("put java_docs {title: 'vector parameter', year: 2026, embed: ?}")) {
                p.setObject(1, new PGvector(new float[] {0.1f, 0.25f, 0.3f}));
                p.executeUpdate();
            }
            try (PreparedStatement p = conn.prepareStatement("get java_docs select title, embed near embed ? limit 1")) {
                p.setObject(1, new PGvector(new float[] {0.1f, 0.2f, 0.3f}));
                ResultSet r = p.executeQuery();
                expect(r.next(), "a nearest row");
                expect(r.getString(1).equals("Night at the oasis"), "the identical vector is nearest");
                float[] v = ((PGvector) r.getObject(2)).toArray();
                expect(v.length == 3 && Math.abs(v[1] - 0.2f) < 1e-6, "the vector as written");
            }
        });
        check("lists the collection through DatabaseMetaData", () -> {
            ResultSet r = conn.getMetaData().getTables(null, "public", "java_docs", null);
            expect(r.next(), "the table");
        });
        System.out.println(failed == 0 ? "all passed" : failed + " failed");
        System.exit(failed == 0 ? 0 : 1);
    }
}
