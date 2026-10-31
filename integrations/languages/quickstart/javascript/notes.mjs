async function query(q, params = []) {
  const res = await fetch('http://127.0.0.1:8080/query', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ query: q, params }),
  });
  const body = await res.json();
  if (!res.ok) throw Object.assign(new Error(body.error), { status: res.status });
  return body;
}

await query('put notes {title: $1, year: $2, kind: $3}', ['Sand and stars', 2023, 'book']);

const rows = await query('get notes where year >= $1 order year desc', [2020]);
console.log('from 2020:', rows.map((r) => r.title).join(', '));

const hits = await query('get notes select title match title $1 fuzzy', ['oasys']);
console.log('search:', hits.map((h) => h.title).join(', '));

try {
  await query('get nowhere');
} catch (e) {
  console.log(`refused: ${e.status} ${e.message}`);
}
