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

query('create collection if not exists docs (title text @text, year int @sorted)');
query('put docs {title: $1, year: $2}', ['Dunes', 2021]);
query('put docs {title: $1, year: $2}', ['Night at the oasis', 2024]);

$rows = query('get docs select title, year where year >= $1 order year desc limit $2', [2020, 5]);
if ($rows !== [['title' => 'Night at the oasis', 'year' => 2024], ['title' => 'Dunes', 'year' => 2021]]) {
    fwrite(STDERR, 'the filtered read answered ' . json_encode($rows) . "\n");
    exit(1);
}

$hits = query('get docs select title match title $1 fuzzy', ['oasys']);
if (array_column($hits, 'title') !== ['Night at the oasis'] || $hits[0]['_score'] <= 0) {
    fwrite(STDERR, 'match answered ' . json_encode($hits) . "\n");
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
