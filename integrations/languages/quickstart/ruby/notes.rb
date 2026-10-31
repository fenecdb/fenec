require "json"
require "net/http"

# One FenecQL statement; the answer is its status and the decoded JSON body.
def query(q, params = [])
  res = Net::HTTP.post(URI("http://127.0.0.1:8080/query"),
                       JSON.generate(query: q, params: params),
                       "Content-Type" => "application/json")
  [res.code.to_i, JSON.parse(res.body)]
end

query("put notes {title: $1, year: $2, kind: $3}", ["Sand and stars", 2023, "book"])

_, rows = query("get notes where year >= $1 order year desc", [2020])
puts "from 2020: #{rows.map { |r| r["title"] }.join(", ")}"

_, hits = query("get notes select title match title $1 fuzzy", ["oasys"])
puts "search: #{hits.map { |h| h["title"] }.join(", ")}"

status, error = query("get nowhere")
puts "refused: #{status} #{error["error"]}"
