# The pg gem and pgvector-ruby against fenec-pg. Each check prints its name;
# a failure is counted and the run exits non-zero.
require "pg"
require "pgvector"

$failed = 0
def check(name)
  yield
  puts "ok   #{name}"
rescue StandardError => e
  $failed += 1
  puts "FAIL #{name}: #{e.message.lines.first}"
end
def expect(cond, what) = (raise what unless cond)

conn = nil
check("connects") { conn = PG.connect(ENV.fetch("FENEC_PG")) }
check("creates a collection") do
  conn.exec("create collection if not exists ruby_docs (title text, year int @hash, embed vector<3> @hnsw(cosine))")
end
check("writes with parameters") do
  r = conn.exec_params("put ruby_docs {title: $1, year: $2, embed: [0.1, 0.2, 0.3]}", ["Night at the oasis", 2024])
  expect(r.cmd_tuples == 1, "one row written")
end
check("reads rows") do
  r = conn.exec_params("get ruby_docs select title, year where year >= $1", [2020])
  expect(r[0]["title"] == "Night at the oasis" && r[0]["year"].to_i == 2024, "its values")
end
check("commits and rolls back") do
  conn.transaction { |c| c.exec("put ruby_docs {title: 'kept', year: 2025, embed: [0.3, 0.2, 0.1]}") }
  begin
    conn.transaction do |c|
      c.exec("put ruby_docs {title: 'dropped', year: 2025, embed: [0.2, 0.2, 0.2]}")
      raise PG::Error, "roll back"
    end
  rescue PG::Error
  end
  n = conn.exec("get ruby_docs where title = 'dropped' count")[0].values.first.to_i
  expect(n.zero?, "the rolled back row is gone")
end
check("writes a vector and searches near one with pgvector's type map") do
  registry = PG::BasicTypeRegistry.new.define_default_types
  Pgvector::PG.register_vector(registry)
  conn.type_map_for_results = PG::BasicTypeMapForResults.new(conn, registry: registry)
  conn.type_map_for_queries = PG::BasicTypeMapForQueries.new(conn, registry: registry)
  conn.exec_params("put ruby_docs {title: 'vector parameter', year: 2026, embed: $1}", [Pgvector::Vector.new([0.1, 0.25, 0.3])])
  r = conn.exec_params("get ruby_docs select title, embed near embed $1 limit 1", [Pgvector::Vector.new([0.1, 0.2, 0.3])])
  expect(r[0]["title"] == "Night at the oasis", "the identical vector is nearest")
  v = r[0]["embed"].to_a
  expect(v.length == 3 && (v[1] - 0.2).abs < 1e-6, "the vector as written")
end
puts $failed.zero? ? "all passed" : "#{$failed} failed"
exit($failed.zero? ? 0 : 1)
