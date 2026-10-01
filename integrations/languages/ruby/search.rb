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

query("create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))")
query("put docs {title: $1, embed: $2}", ["Night at the oasis", [0.1, 0.2, 0.3]])
query("put docs {title: $1, embed: $2}", ["Dunes", [0.9, 0.1, 0.0]])

rows = query("get docs select title near embed $1 limit 5", [[0.1, 0.2, 0.3]])
titles = rows.map { |r| r["title"] }
abort "near answered #{rows}" unless titles == ["Night at the oasis", "Dunes"]

begin
  query("get nowhere")
  abort "a missing collection was answered"
rescue FenecError => e
  raise unless e.status == 404
end

puts "ruby: ok"
