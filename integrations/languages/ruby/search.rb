# Ruby with Net::HTTP and json from the standard library. Run by
# ../run-tests.sh.
require "json"
require "net/http"

URL = URI(ENV.fetch("FENEC_URL", "http://127.0.0.1:8080"))
TOKEN = ENV["FENEC_TOKEN"]

class FenecError < StandardError
  attr_reader :status

  def initialize(message, status)
    super(message)
    @status = status
  end
end

# One FenecQL statement; a refusal raises with the server's message and status.
def query(q, params = [])
  req = Net::HTTP::Post.new(URI.join(URL, "/query"), "Content-Type" => "application/json")
  req["Authorization"] = "Bearer #{TOKEN}" if TOKEN
  req.body = JSON.generate(query: q, params: params)
  res = Net::HTTP.start(URL.host, URL.port) { |http| http.request(req) }
  body = JSON.parse(res.body)
  raise FenecError.new(body["error"], res.code.to_i) unless res.is_a?(Net::HTTPSuccess)
  body
end

query("create collection if not exists docs (title text @text, year int @sorted)")
query("put docs {title: $1, year: $2}", ["Dunes", 2021])
query("put docs {title: $1, year: $2}", ["Night at the oasis", 2024])

rows = query("get docs select title, year where year >= $1 order year desc limit $2", [2020, 5])
expected = [{ "title" => "Night at the oasis", "year" => 2024 }, { "title" => "Dunes", "year" => 2021 }]
abort "the filtered read answered #{rows}" unless rows == expected

hits = query("get docs select title match title $1 fuzzy", ["oasys"])
abort "match answered #{hits}" unless hits.map { |h| h["title"] } == ["Night at the oasis"] && hits[0]["_score"] > 0

begin
  query("get nowhere")
  abort "a missing collection was answered"
rescue FenecError => e
  raise unless e.status == 404
end

puts "ruby: ok"
