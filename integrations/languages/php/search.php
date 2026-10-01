<?php
// PHP with the curl extension and json_encode alone. Run by ../run-tests.sh.

$url = getenv('FENEC_URL') ?: 'http://127.0.0.1:8080';
$token = getenv('FENEC_TOKEN') ?: null;

// One FenecQL statement; a refusal throws with the server's message and status.
function query(string $q, array $params = []) {
    global $url, $token;
    $ch = curl_init("$url/query");
    curl_setopt_array($ch, [
        CURLOPT_POST => true,
        CURLOPT_RETURNTRANSFER => true,
        CURLOPT_HTTPHEADER => array_filter([
            'Content-Type: application/json',
            $token ? "Authorization: Bearer $token" : null,
        ]),
        CURLOPT_POSTFIELDS => json_encode(['query' => $q, 'params' => $params]),
    ]);
    $body = json_decode(curl_exec($ch), true);
    $status = curl_getinfo($ch, CURLINFO_RESPONSE_CODE);
    if ($status >= 300) {
        throw new RuntimeException($body['error'], $status);
    }
    return $body;
}

query('create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))');
query('put docs {title: $1, embed: $2}', ['Night at the oasis', [0.1, 0.2, 0.3]]);
query('put docs {title: $1, embed: $2}', ['Dunes', [0.9, 0.1, 0.0]]);

$rows = query('get docs select title near embed $1 limit 5', [[0.1, 0.2, 0.3]]);
$titles = array_column($rows, 'title');
if ($titles !== ['Night at the oasis', 'Dunes']) {
    fwrite(STDERR, 'near answered ' . json_encode($rows) . "\n");
    exit(1);
}

try {
    query('get nowhere');
    fwrite(STDERR, "a missing collection was answered\n");
    exit(1);
} catch (RuntimeException $e) {
    if ($e->getCode() !== 404) {
        throw $e;
    }
}

echo "php: ok\n";
