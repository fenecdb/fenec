//! Points, fenecdb against PostGIS and Redis: `make geo-bench`.
//!
//! ```text
//! cargo run --release -p fenec-bench --bin geo -- [--rows N] [--queries Q]
//!     [--only fenec,pg,redis] [--dist cities,uniform] [--pause S] [--out FILE]
//! ```
//!
//! N points (a million unless given) in two spreads: `cities`, crowded
//! round 48 real cities -- each a gaussian of its own width, the larger
//! cities taking more -- with 5% of them anywhere, as a shop's or a
//! courier's data lies; and `uniform`, evenly over the earth's area. Every
//! latitude stays within 85 degrees, which Redis refuses past. Each query
//! is a point near one of the points, jittered, for `cities`, and anywhere
//! for `uniform`.
//!
//! Each system is asked what Redis's GEOSEARCH asks: every row within 100
//! m, 1 km, 10 km and 100 km of a point, its ids answered whole; the ten
//! nearest within 50 km, nearest first (`BYRADIUS 50 km ASC COUNT 10`);
//! and the ten nearest anywhere. Latencies are a single client's, p50 and
//! p99, each query asked once to warm and once measured.
//!
//!   * `fenec`: fenec-core in process, a statement parsed once and asked
//!     with its parameters: `get p select id where distance(loc, $1) <=
//!     $2`, `... near loc $1 limit 10`. Over a collection with `@geo` and
//!     its twin without, whose scan is the reference the index answers as.
//!     The index's build is timed from the documents of a reopened file
//!     (`warm_index`, what the first read after an open does), and its
//!     memory is what the engine counts of it; a put's cost with the
//!     index and without, in turns.
//!   * `pg`: PostgreSQL 17 + PostGIS 3.5 in a container (`imresamu/postgis`,
//!     its maintainer's build for arm64: `postgis/postgis` is amd64 alone and
//!     would run emulated), a `geography(Point, 4326)` column loaded by
//!     COPY, a GiST index built after, `ST_DWithin(loc, $1, $2, false)` --
//!     on the sphere, as fenecdb and Redis measure, where PostGIS defaults
//!     to the spheroid -- and `ORDER BY loc <-> $1 LIMIT 10`. Statements
//!     prepared once. `pg_stat_statements` gives the mean time inside the
//!     server, its planning and execution, and the plans are printed.
//!     PostGIS's sphere is the mean radius, 6 371 008.8 m, Redis's and
//!     fenecdb's 6 372 797.6, so a radius holds a few more of its rows.
//!   * `redis`: Redis 7 in a container, `GEOADD` in pipelines of 1 000,
//!     `GEOSEARCH key FROMLONLAT lon lat BYRADIUS r m [ASC COUNT 10]`. Its
//!     `INFO commandstats` gives the mean time inside the server.
//!
//! fenecdb answers in process; PostgreSQL and Redis from Docker's virtual
//! machine, through its port forwarding, which costs every request about
//! 0.2 ms (`make roundtrip-bench`). So beside each client's latency stands
//! each server's own mean time of the same query, which is the engines'
//! comparison. The containers take turns, each removed with its volume,
//! and are held to 3 GB of the Docker VM's 4 (PostgreSQL's shared buffers
//! 1 GB); the VM's 8 cores are theirs.
//!
//! Redis's `GEODIST` is checked against fenecdb's `distance` on a sample:
//! from the points Redis keeps (`GEOPOS`, a 52-bit geohash's cell) they
//! must agree to Redis's 0.1 mm of output, and from the points as written
//! to what the cells move two points by, 0.67 m at most at the equator.

use postgres::{Client, NoTls};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use fenec_core::prelude::*;

const PG: &str = "host=127.0.0.1 port=55434 user=postgres password=fenec dbname=geo";
const REDIS: &str = "127.0.0.1:56379";
const RADII: [f64; 4] = [100.0, 1_000.0, 10_000.0, 100_000.0];
const NEAR_RADIUS: f64 = 50_000.0;

// ------------------------------------------------------------ randomness

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// A standard normal, by Box-Muller.
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit().max(1e-300), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// Cities: longitude, latitude, and how far round them their points lie
/// (one standard deviation, km). Weighted by their order, the first ones
/// heavier.
const CITIES: [(f64, f64, f64); 48] = [
    (139.6917, 35.6895, 30.0),
    (77.2090, 28.6139, 25.0),
    (121.4737, 31.2304, 25.0),
    (-46.6333, -23.5505, 25.0),
    (-99.1332, 19.4326, 20.0),
    (31.2357, 30.0444, 15.0),
    (72.8777, 19.0760, 15.0),
    (116.4074, 39.9042, 25.0),
    (90.4125, 23.8103, 10.0),
    (135.5022, 34.6937, 15.0),
    (-74.0060, 40.7128, 20.0),
    (67.0011, 24.8607, 15.0),
    (-58.3816, -34.6037, 20.0),
    (28.9784, 41.0082, 20.0),
    (88.3639, 22.5726, 12.0),
    (120.9842, 14.5995, 12.0),
    (3.3792, 6.5244, 15.0),
    (-43.1729, -22.9068, 15.0),
    (113.2644, 23.1291, 20.0),
    (-118.2437, 34.0522, 30.0),
    (37.6173, 55.7558, 20.0),
    (2.3522, 48.8566, 15.0),
    (-0.1276, 51.5072, 20.0),
    (106.8456, -6.2088, 15.0),
    (100.5018, 13.7563, 15.0),
    (126.9780, 37.5665, 15.0),
    (-77.0428, -12.0464, 12.0),
    (13.4050, 52.5200, 12.0),
    (-87.6298, 41.8781, 20.0),
    (-79.3832, 43.6532, 15.0),
    (-3.7038, 40.4168, 10.0),
    (12.4964, 41.9028, 10.0),
    (151.2093, -33.8688, 20.0),
    (144.9631, -37.8136, 20.0),
    (18.4241, -33.9249, 12.0),
    (36.8219, -1.2921, 10.0),
    (55.2708, 25.2048, 15.0),
    (103.8198, 1.3521, 8.0),
    (-123.1216, 49.2827, 10.0),
    (-122.4194, 37.7749, 12.0),
    (4.9041, 52.3676, 8.0),
    (18.0686, 59.3293, 8.0),
    (32.8597, 39.9334, 10.0),
    (174.7633, -36.8485, 8.0),
    (-70.6693, -33.4489, 12.0),
    (-157.8583, 21.3069, 6.0),
    (178.4419, -18.1248, 4.0),
    (-179.0, 60.0, 30.0),
];

/// A point of the spread, its latitude within 85 degrees.
fn point(r: &mut Rng, dist: &str) -> (f64, f64) {
    let anywhere = |r: &mut Rng| {
        // Even over the area: the latitude's sine uniform.
        let lat = (r.unit() * 2.0 - 1.0).asin().to_degrees();
        (r.unit() * 360.0 - 180.0, lat.clamp(-85.0, 85.0))
    };
    if dist == "uniform" || r.unit() < 0.05 {
        return anywhere(r);
    }
    // The first cities heavier: the index of a city drawn as a square.
    let i = ((r.unit() * r.unit()) * CITIES.len() as f64) as usize;
    let (lon, lat, km) = CITIES[i.min(CITIES.len() - 1)];
    let dy = r.normal() * km / 111.32;
    let dx = r.normal() * km / (111.32 * lat.to_radians().cos().max(0.05));
    let lon = (lon + dx + 540.0).rem_euclid(360.0) - 180.0;
    (lon, (lat + dy).clamp(-85.0, 85.0))
}

fn points(n: usize, dist: &str) -> Vec<(f64, f64)> {
    let mut r = Rng(0x6765_6F00 ^ dist.len() as u64);
    (0..n).map(|_| point(&mut r, dist)).collect()
}

/// The queries' points: near a point of the data, as an app asks, or
/// anywhere for the even spread.
fn queries(data: &[(f64, f64)], q: usize, dist: &str) -> Vec<(f64, f64)> {
    let mut r = Rng(0x7175_6572 ^ dist.len() as u64);
    (0..q)
        .map(|_| match dist {
            "uniform" => point(&mut r, dist),
            _ => {
                let p = data[(r.next() % data.len() as u64) as usize];
                let lon = (p.0 + r.normal() * 0.01 + 540.0).rem_euclid(360.0) - 180.0;
                (lon, (p.1 + r.normal() * 0.01).clamp(-85.0, 85.0))
            }
        })
        .collect()
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// One line of results, to stdout and the file.
struct Out(Option<std::fs::File>);

impl Out {
    fn line(&mut self, cells: &[String]) {
        let line = cells.join("\t");
        println!("{line}");
        if let Some(f) = &mut self.0 {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Latencies of a set of queries, in microseconds, and the rows answered.
struct Timed {
    us: Vec<f64>,
    rows: u64,
}

impl Timed {
    fn report(&mut self, out: &mut Out, head: &[&str], server_us: Option<f64>) {
        let n = self.us.len().max(1) as f64;
        let mut cells: Vec<String> = head.iter().map(|s| s.to_string()).collect();
        cells.push(format!("p50={:.1}us", pct(&mut self.us, 0.5)));
        cells.push(format!("p99={:.1}us", pct(&mut self.us, 0.99)));
        cells.push(format!("rows={:.1}", self.rows as f64 / n));
        if let Some(s) = server_us {
            cells.push(format!("server_mean={s:.1}us"));
        }
        out.line(&cells);
    }
}

fn time(qs: &[(f64, f64)], mut ask: impl FnMut((f64, f64)) -> u64) -> Timed {
    for &q in qs.iter().take(qs.len().min(50)) {
        ask(q);
    }
    let mut t = Timed {
        us: Vec::with_capacity(qs.len()),
        rows: 0,
    };
    for &q in qs {
        let at = Instant::now();
        t.rows += ask(q);
        t.us.push(at.elapsed().as_secs_f64() * 1e6);
    }
    t
}

// ------------------------------------------------------------------ fenec

fn pt(p: (f64, f64)) -> Value {
    Value::List(vec![Value::Float(p.0), Value::Float(p.1)])
}

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
}

fn load(db: &mut Database, data: &[(f64, f64)]) -> Duration {
    let at = Instant::now();
    for chunk in data.chunks(10_000) {
        let docs = chunk
            .iter()
            .map(|p| vec![("loc".to_string(), Expr::Lit(pt(*p)))])
            .collect();
        db.execute(&Statement::Put {
            collection: "p".into(),
            docs,
            docs_param: None,
            insert: false,
            if_absent: false,
            else_set: None,
            require: None,
        })
        .unwrap();
    }
    at.elapsed()
}

fn rows_of(r: Response) -> u64 {
    r.rows().map_or(0, |r| r.rows.len() as u64)
}

fn fenec(out: &mut Out, dist: &str, data: &[(f64, f64)], qs: &[(f64, f64)]) {
    let n = data.len();
    let mut ix = Database::new();
    exec(&mut ix, "create collection p (loc geo @geo)");
    let took = load(&mut ix, data);
    out.line(&[
        "fenec".into(),
        dist.into(),
        "load @geo".into(),
        format!("{:.0} rows/s", n as f64 / took.as_secs_f64()),
    ]);
    let mut plain = Database::new();
    exec(&mut plain, "create collection p (loc geo)");
    let took = load(&mut plain, data);
    out.line(&[
        "fenec".into(),
        dist.into(),
        "load no index".into(),
        format!("{:.0} rows/s", n as f64 / took.as_secs_f64()),
    ]);

    // The build from the documents, as the first read after an open makes it.
    let mut builds = Vec::new();
    let image = ix.snapshot();
    let mut grown = 0;
    for _ in 0..3 {
        let mut db = Database::new();
        db.load(&image).unwrap();
        let before = db.memory_bytes();
        let at = Instant::now();
        db.warm_index("p", "loc").unwrap();
        builds.push(at.elapsed().as_secs_f64() * 1e3);
        grown = db.memory_bytes() - before;
    }
    out.line(&[
        "fenec".into(),
        dist.into(),
        "index build".into(),
        format!("{:.1} ms", pct(&mut builds, 0.5)),
        format!("{:.1} MB", grown as f64 / 1e6),
    ]);
    drop(image);

    let radius = fenec_ql::parse_one("get p select id where distance(loc, $1) <= $2").unwrap();
    for r in RADII {
        let mut t = time(qs, |q| {
            rows_of(ix.query(&radius, &[pt(q), Value::Float(r)]).unwrap())
        });
        t.report(
            out,
            &["fenec", dist, &format!("radius {r} m"), "index"],
            None,
        );
        let few = &qs[..qs.len().min(40)];
        let mut s = time(few, |q| {
            rows_of(plain.query(&radius, &[pt(q), Value::Float(r)]).unwrap())
        });
        s.report(
            out,
            &["fenec", dist, &format!("radius {r} m"), "scan"],
            None,
        );
    }
    let near50 =
        fenec_ql::parse_one("get p select id where distance(loc, $1) <= $2 near loc $1 limit 10")
            .unwrap();
    let mut t = time(qs, |q| {
        rows_of(
            ix.query(&near50, &[pt(q), Value::Float(NEAR_RADIUS)])
                .unwrap(),
        )
    });
    t.report(
        out,
        &["fenec", dist, "nearest 10 within 50 km", "index"],
        None,
    );
    let near = fenec_ql::parse_one("get p select id near loc $1 limit 10").unwrap();
    let mut t = time(qs, |q| rows_of(ix.query(&near, &[pt(q)]).unwrap()));
    t.report(out, &["fenec", dist, "nearest 10", "index"], None);
    let exact = fenec_ql::parse_one("get p select id near loc $1 exact limit 10").unwrap();
    let few = &qs[..qs.len().min(40)];
    let mut t = time(few, |q| rows_of(ix.query(&exact, &[pt(q)]).unwrap()));
    t.report(out, &["fenec", dist, "nearest 10", "scan"], None);

    // A put's cost with the index and without, single puts into the
    // loaded collections in turns -- and beside them an ordered index's
    // over as many random floats, the price of keeping a key in order.
    let mut sorted = Database::new();
    exec(&mut sorted, "create collection s (f float @sorted)");
    let mut r = Rng(7);
    for chunk in (0..n).collect::<Vec<_>>().chunks(10_000) {
        let docs = chunk
            .iter()
            .map(|_| vec![("f".to_string(), Expr::Lit(Value::Float(r.unit())))])
            .collect();
        sorted
            .execute(&Statement::Put {
                collection: "s".into(),
                docs,
                docs_param: None,
                insert: false,
                if_absent: false,
                else_set: None,
                require: None,
            })
            .unwrap();
    }
    sorted.warm_index("s", "f").unwrap();
    let put = fenec_ql::parse_one("put p {loc: $1}").unwrap();
    let put_f = fenec_ql::parse_one("put s {f: $1}").unwrap();
    let mut r = Rng(9);
    let mut ns = [Vec::new(), Vec::new(), Vec::new()];
    for round in 0..9 {
        let at = Instant::now();
        for _ in 0..20_000 {
            match round % 3 {
                0 => ix.execute_with(&put, &[pt(point(&mut r, dist))]),
                1 => plain.execute_with(&put, &[pt(point(&mut r, dist))]),
                _ => sorted.execute_with(&put_f, &[Value::Float(r.unit())]),
            }
            .unwrap();
        }
        ns[round % 3].push(at.elapsed().as_secs_f64() * 1e9 / 20_000.0);
    }
    out.line(&[
        "fenec".into(),
        dist.into(),
        "put".into(),
        format!("@geo {:.0} ns", pct(&mut ns[0], 0.5)),
        format!("no index {:.0} ns", pct(&mut ns[1], 0.5)),
        format!("@sorted float {:.0} ns", pct(&mut ns[2], 0.5)),
    ]);
}

// --------------------------------------------------------------- docker

/// A container for a system's turn, removed with its volume after.
struct Container(String);

impl Container {
    fn start(name: &str, args: &[&str]) -> Container {
        let c = Container(format!("fenecgeo-{name}"));
        c.remove();
        let st = std::process::Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &c.0])
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "docker run {args:?}");
        c
    }
    fn remove(&self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", "-v", &self.0])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        self.remove();
    }
}

// --------------------------------------------------------------- postgis

fn pg(out: &mut Out, dist: &str, data: &[(f64, f64)], qs: &[(f64, f64)]) {
    let _c = Container::start(
        "pg",
        &[
            "-e",
            "POSTGRES_PASSWORD=fenec",
            "-e",
            "POSTGRES_DB=geo",
            "-p",
            "127.0.0.1:55434:5432",
            "--memory=3g",
            "--shm-size=1g",
            "imresamu/postgis:17-3.5",
            "-c",
            "shared_buffers=1GB",
            "-c",
            "maintenance_work_mem=512MB",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track_planning=on",
            "-c",
            "synchronous_commit=off",
        ],
    );
    let mut c = loop {
        // The image's entrypoint starts PostgreSQL, stops it and starts it
        // again once PostGIS is in: wait for the extension.
        if let Ok(mut c) = Client::connect(PG, NoTls) {
            if c.simple_query("CREATE EXTENSION IF NOT EXISTS postgis")
                .is_ok()
            {
                break c;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    c.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
         CREATE TABLE p (id bigint PRIMARY KEY, loc geography(Point, 4326) NOT NULL);",
    )
    .unwrap();
    let at = Instant::now();
    {
        let mut w = c.copy_in("COPY p (id, loc) FROM STDIN").unwrap();
        let mut buf = String::with_capacity(64 << 20);
        for (i, (lon, lat)) in data.iter().enumerate() {
            use std::fmt::Write as _;
            let _ = writeln!(buf, "{}\tSRID=4326;POINT({lon} {lat})", i + 1);
            if buf.len() > 60 << 20 {
                w.write_all(buf.as_bytes()).unwrap();
                buf.clear();
            }
        }
        w.write_all(buf.as_bytes()).unwrap();
        w.finish().unwrap();
    }
    let loaded = at.elapsed();
    let at = Instant::now();
    c.batch_execute("CREATE INDEX p_loc ON p USING GIST (loc); ANALYZE p;")
        .unwrap();
    let built = at.elapsed();
    let size: i64 = c
        .query_one("SELECT pg_relation_size('p_loc')", &[])
        .unwrap()
        .get(0);
    out.line(&[
        "pg".into(),
        dist.into(),
        "load (COPY)".into(),
        format!("{:.0} rows/s", data.len() as f64 / loaded.as_secs_f64()),
    ]);
    out.line(&[
        "pg".into(),
        dist.into(),
        "index build".into(),
        format!("{:.1} ms", built.as_secs_f64() * 1e3),
        format!("{:.1} MB", size as f64 / 1e6),
    ]);

    let point = "ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography";
    let radius = c
        .prepare(&format!(
            "SELECT id FROM p WHERE ST_DWithin(loc, {point}, $3, false)"
        ))
        .unwrap();
    let near50 = c
        .prepare(&format!(
            "SELECT id FROM p WHERE ST_DWithin(loc, {point}, $3, false) \
             ORDER BY loc <-> {point} LIMIT 10"
        ))
        .unwrap();
    let near = c
        .prepare(&format!(
            "SELECT id FROM p ORDER BY loc <-> {point} LIMIT 10"
        ))
        .unwrap();
    // Planning and execution both: PostgreSQL plans a prepared statement
    // again each time while its generic plan looks costlier than the custom
    // ones, as PostGIS's estimates make it here.
    let mean = |c: &mut Client, like: &str| -> Option<f64> {
        let row = c
            .query_opt(
                "SELECT sum(total_exec_time + total_plan_time) / sum(calls) \
                 FROM pg_stat_statements WHERE query LIKE $1",
                &[&like],
            )
            .ok()??;
        row.get::<_, Option<f64>>(0).map(|ms| ms * 1e3)
    };
    let reset = |c: &mut Client| {
        c.batch_execute("SELECT pg_stat_statements_reset()")
            .unwrap();
    };
    for r in RADII {
        reset(&mut c);
        let mut t = time(qs, |q| {
            c.query(&radius, &[&q.0, &q.1, &r]).unwrap().len() as u64
        });
        let server = mean(&mut c, "SELECT id FROM p WHERE ST_DWithin%");
        t.report(out, &["pg", dist, &format!("radius {r} m"), "gist"], server);
    }
    reset(&mut c);
    let mut t = time(qs, |q| {
        c.query(&near50, &[&q.0, &q.1, &NEAR_RADIUS]).unwrap().len() as u64
    });
    // pg_stat_statements writes the `LIMIT 10` as a parameter.
    let server = mean(&mut c, "SELECT id FROM p WHERE ST_DWithin%ORDER BY%");
    t.report(
        out,
        &["pg", dist, "nearest 10 within 50 km", "gist"],
        server,
    );
    reset(&mut c);
    let mut t = time(qs, |q| c.query(&near, &[&q.0, &q.1]).unwrap().len() as u64);
    let server = mean(&mut c, "SELECT id FROM p ORDER BY loc <->%");
    t.report(out, &["pg", dist, "nearest 10", "gist"], server);

    // The plans the statements ran by, once the generic plan has taken
    // over (after five executions): that each reads the GiST index.
    c.batch_execute(&format!(
        "PREPARE r(float8, float8, float8) AS \
         SELECT id FROM p WHERE ST_DWithin(loc, {point}, $3, false)"
    ))
    .unwrap();
    let q = qs[0];
    for r in [100.0, 10_000.0] {
        for _ in 0..6 {
            c.batch_execute(&format!("EXECUTE r({}, {}, {r})", q.0, q.1))
                .unwrap();
        }
        let plan = c
            .simple_query(&format!(
                "EXPLAIN (ANALYZE, BUFFERS) EXECUTE r({}, {}, {r})",
                q.0, q.1
            ))
            .unwrap();
        for m in plan {
            if let postgres::SimpleQueryMessage::Row(row) = m {
                out.line(&[format!("# pg plan {r} m: {}", row.get(0).unwrap_or(""))]);
            }
        }
    }
    // Palermo to Catania on the sphere and on the ellipsoid, for the page.
    let row = c
        .query_one(
            "SELECT ST_Distance(a, b, false), ST_Distance(a, b) FROM \
             (SELECT 'SRID=4326;POINT(13.361389 38.115556)'::geography a, \
                     'SRID=4326;POINT(15.087269 37.502669)'::geography b) x",
            &[],
        )
        .unwrap();
    let (sphere, spheroid): (f64, f64) = (row.get(0), row.get(1));
    out.line(&[
        "pg".into(),
        "check".into(),
        "Palermo to Catania".into(),
        format!("sphere {sphere:.4} m"),
        format!("spheroid {spheroid:.4} m"),
        format!(
            "distance() {:.4} m",
            fenec_core::geo::distance((13.361389, 38.115556), (15.087269, 37.502669))
        ),
    ]);
}

// ----------------------------------------------------------------- redis

/// A connection speaking RESP, pipelined.
struct Redis {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

#[derive(Debug)]
enum Reply {
    Int,
    Text(Option<String>),
    List(Vec<Reply>),
}

impl Redis {
    fn connect() -> Redis {
        for _ in 0..200 {
            if let Ok(s) = TcpStream::connect(REDIS) {
                s.set_nodelay(true).unwrap();
                let mut c = Redis {
                    r: BufReader::with_capacity(1 << 20, s.try_clone().unwrap()),
                    w: s,
                };
                if c.call(&["PING"]).is_some() {
                    return c;
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("redis did not come up");
    }

    fn send(&mut self, args: &[&str], buf: &mut Vec<u8>) {
        buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
        for a in args {
            buf.extend_from_slice(format!("${}\r\n{a}\r\n", a.len()).as_bytes());
        }
    }

    fn read(&mut self) -> Option<Reply> {
        let mut line = String::new();
        // Docker's port takes a connection before Redis is up, and closes
        // it: nothing read is no reply, and `connect` tries again.
        if self.r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        let (kind, rest) = line.split_at(1);
        Some(match kind {
            "+" => Reply::Text(Some(rest.to_string())),
            "-" => panic!("redis: {rest}"),
            ":" => Reply::Int,
            "$" => {
                let n: i64 = rest.parse().unwrap();
                if n < 0 {
                    Reply::Text(None)
                } else {
                    let mut b = vec![0u8; n as usize + 2];
                    self.r.read_exact(&mut b).unwrap();
                    b.truncate(n as usize);
                    Reply::Text(Some(String::from_utf8(b).unwrap()))
                }
            }
            "*" => {
                let n: i64 = rest.parse().unwrap();
                let mut v = Vec::with_capacity(n.max(0) as usize);
                for _ in 0..n.max(0) {
                    v.push(self.read()?);
                }
                Reply::List(v)
            }
            other => panic!("redis: unknown reply {other}"),
        })
    }

    fn call(&mut self, args: &[&str]) -> Option<Reply> {
        let mut buf = Vec::new();
        self.send(args, &mut buf);
        self.w.write_all(&buf).ok()?;
        self.read()
    }

    fn info(&mut self, section: &str) -> String {
        match self.call(&["INFO", section]) {
            Some(Reply::Text(Some(t))) => t,
            other => panic!("INFO: {other:?}"),
        }
    }
}

/// `usec_per_call` of a command in `INFO commandstats`.
fn per_call(info: &str, cmd: &str) -> Option<f64> {
    let line = info
        .lines()
        .find(|l| l.starts_with(&format!("cmdstat_{cmd}:")))?;
    let v = line
        .split(',')
        .find(|kv| kv.starts_with("usec_per_call="))?;
    v["usec_per_call=".len()..].parse().ok()
}

fn redis(out: &mut Out, dist: &str, data: &[(f64, f64)], qs: &[(f64, f64)]) {
    let _c = Container::start(
        "redis",
        &[
            "-p",
            "127.0.0.1:56379:6379",
            "--memory=3g",
            "redis:7",
            "redis-server",
            "--save",
            "",
            "--appendonly",
            "no",
        ],
    );
    let mut r = Redis::connect();
    let used = |r: &mut Redis| -> f64 {
        let info = r.info("memory");
        info.lines()
            .find_map(|l| l.strip_prefix("used_memory:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let before = used(&mut r);
    let at = Instant::now();
    for (b, chunk) in data.chunks(1_000).enumerate() {
        let mut args: Vec<String> = vec!["GEOADD".into(), "p".into()];
        for (i, (lon, lat)) in chunk.iter().enumerate() {
            args.push(format!("{lon}"));
            args.push(format!("{lat}"));
            args.push(format!("{}", b * 1_000 + i + 1));
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        r.call(&refs).unwrap();
    }
    let loaded = at.elapsed();
    let grown = used(&mut r) - before;
    out.line(&[
        "redis".into(),
        dist.into(),
        "load (GEOADD)".into(),
        format!("{:.0} rows/s", data.len() as f64 / loaded.as_secs_f64()),
        format!("{:.1} MB", grown / 1e6),
    ]);
    let count = |reply: Option<Reply>| match reply {
        Some(Reply::List(v)) => v.len() as u64,
        other => panic!("GEOSEARCH: {other:?}"),
    };
    let search = |r: &mut Redis, q: (f64, f64), metres: f64, nearest: bool| {
        let (lon, lat, m) = (format!("{}", q.0), format!("{}", q.1), format!("{metres}"));
        let mut args = vec![
            "GEOSEARCH",
            "p",
            "FROMLONLAT",
            &lon,
            &lat,
            "BYRADIUS",
            &m,
            "m",
        ];
        if nearest {
            args.extend(["ASC", "COUNT", "10"]);
        }
        r.call(&args)
    };
    for m in RADII {
        r.call(&["CONFIG", "RESETSTAT"]);
        let mut t = time(qs, |q| count(search(&mut r, q, m, false)));
        let server = per_call(&r.info("commandstats"), "geosearch");
        t.report(
            out,
            &["redis", dist, &format!("radius {m} m"), "zset"],
            server,
        );
    }
    r.call(&["CONFIG", "RESETSTAT"]);
    let mut t = time(qs, |q| count(search(&mut r, q, NEAR_RADIUS, true)));
    let server = per_call(&r.info("commandstats"), "geosearch");
    t.report(
        out,
        &["redis", dist, "nearest 10 within 50 km", "zset"],
        server,
    );
    // The ten nearest anywhere: a radius past half the earth, every
    // member measured and sorted -- a few queries.
    r.call(&["CONFIG", "RESETSTAT"]);
    let few = &qs[..qs.len().min(20)];
    let mut t = time(few, |q| count(search(&mut r, q, 2.1e7, true)));
    let server = per_call(&r.info("commandstats"), "geosearch");
    t.report(out, &["redis", dist, "nearest 10", "zset"], server);

    if dist == "cities" {
        check_geodist(out, &mut r);
    }
}

/// Redis's GEODIST against fenecdb's `distance`, on a sample.
fn check_geodist(out: &mut Out, r: &mut Redis) {
    let mut rng = Rng(0xD157);
    let pts: Vec<(f64, f64)> = (0..1_000)
        .map(|i| match i % 2 {
            0 => point(&mut rng, "cities"),
            _ => point(&mut rng, "uniform"),
        })
        .collect();
    let mut args: Vec<String> = vec!["GEOADD".into(), "check".into()];
    for (i, (lon, lat)) in pts.iter().enumerate() {
        args.extend([format!("{lon}"), format!("{lat}"), format!("m{i}")]);
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    r.call(&refs).unwrap();
    // Where Redis keeps each: the centre of its geohash's cell.
    let mut args = vec!["GEOPOS".to_string(), "check".into()];
    args.extend((0..pts.len()).map(|i| format!("m{i}")));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some(Reply::List(pos)) = r.call(&refs) else {
        panic!("GEOPOS")
    };
    let kept: Vec<(f64, f64)> = pos
        .into_iter()
        .map(|p| match p {
            Reply::List(v) => match (&v[0], &v[1]) {
                (Reply::Text(Some(a)), Reply::Text(Some(b))) => {
                    (a.parse().unwrap(), b.parse().unwrap())
                }
                _ => panic!("GEOPOS"),
            },
            _ => panic!("GEOPOS"),
        })
        .collect();
    let (mut kept_max, mut written_max, mut kept_sum) = (0f64, 0f64, 0f64);
    let pairs = 2_000;
    for _ in 0..pairs {
        let (a, b) = (
            (rng.next() % pts.len() as u64) as usize,
            (rng.next() % pts.len() as u64) as usize,
        );
        let Some(Reply::Text(Some(d))) =
            r.call(&["GEODIST", "check", &format!("m{a}"), &format!("m{b}"), "m"])
        else {
            panic!("GEODIST")
        };
        let d: f64 = d.parse().unwrap();
        let mine = fenec_core::geo::distance(kept[a], kept[b]);
        let written = fenec_core::geo::distance(pts[a], pts[b]);
        kept_max = kept_max.max((mine - d).abs());
        kept_sum += (mine - d).abs();
        written_max = written_max.max((written - d).abs());
    }
    out.line(&[
        "redis".into(),
        "check".into(),
        "GEODIST against distance()".into(),
        format!("{pairs} pairs"),
        format!(
            "from GEOPOS: max {kept_max:.6} m, mean {:.6} m",
            kept_sum / pairs as f64
        ),
        format!("from the points written: max {written_max:.4} m"),
    ]);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let rows: usize = opt("--rows").map_or(1_000_000, |v| v.parse().unwrap());
    let q: usize = opt("--queries").map_or(1_000, |v| v.parse().unwrap());
    let only = opt("--only").unwrap_or_else(|| "fenec,pg,redis".into());
    let dists = opt("--dist").unwrap_or_else(|| "cities,uniform".into());
    let mut out = Out(opt("--out").map(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap()
    }));
    out.line(&[format!(
        "# geo-bench rows={rows} queries={q} only={only} dist={dists}"
    )]);
    // Idle between the systems: the fanless laptop this is measured on slows
    // under minutes of load, and the one after a turn would run hot.
    let pause: u64 = opt("--pause").map_or(0, |v| v.parse().unwrap());
    for dist in dists.split(',') {
        let data = points(rows, dist);
        let qs = queries(&data, q, dist);
        for system in only.split(',') {
            std::thread::sleep(Duration::from_secs(pause));
            match system {
                "fenec" => fenec(&mut out, dist, &data, &qs),
                "pg" => pg(&mut out, dist, &data, &qs),
                "redis" => redis(&mut out, dist, &data, &qs),
                other => panic!("no system `{other}`: fenec, pg, redis"),
            }
        }
    }
}
