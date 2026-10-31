<?php

// One FenecQL statement; the answer is its status and the decoded JSON body.
function query(string $q, array $params = []): array {
    $ch = curl_init('http://127.0.0.1:8080/query');
    curl_setopt_array($ch, [
        CURLOPT_POST => true,
        CURLOPT_RETURNTRANSFER => true,
        CURLOPT_HTTPHEADER => ['Content-Type: application/json'],
        CURLOPT_POSTFIELDS => json_encode(['query' => $q, 'params' => $params]),
    ]);
    $body = json_decode(curl_exec($ch), true);
    return [curl_getinfo($ch, CURLINFO_RESPONSE_CODE), $body];
}

query('put notes {title: $1, year: $2, kind: $3}', ['Sand and stars', 2023, 'book']);

[, $rows] = query('get notes where year >= $1 order year desc', [2020]);
echo 'from 2020: ', implode(', ', array_column($rows, 'title')), "\n";

[, $hits] = query('get notes select title match title $1 fuzzy', ['oasys']);
echo 'search: ', implode(', ', array_column($hits, 'title')), "\n";

[$status, $error] = query('get nowhere');
echo 'refused: ', $status, ' ', $error['error'], "\n";
