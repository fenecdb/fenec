// fenecdb's MCP server held to the official TypeScript client
// (@modelcontextprotocol/client): `npm test` here, after `npm ci`. Every
// tool called over every way an agent reaches it -- Streamable HTTP of a
// fenec-server started here, stdio of `fenec mcp` over a file, and stdio
// bridged to the server -- in both eras the client speaks: the handshake
// revision it opens with by default, and 2026-07-28 pinned and probed.
// Then every configuration site/content/docs/mcp.html gives -- Claude
// Code's, Cursor's, Claude Desktop's -- loaded as its client loads it.
// Needs the debug binaries: cargo build -p fenec-server -p fenec-cli.

import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Client, StreamableHTTPClientTransport } from '@modelcontextprotocol/client';
import { StdioClientTransport } from '@modelcontextprotocol/client/stdio';

const root = new URL('../../', import.meta.url);
const server = new URL('target/debug/fenec-server', root).pathname;
const shell = new URL('target/debug/fenec', root).pathname;
const TOKEN = 'mcp-integration-token';
const dir = await mkdtemp(join(tmpdir(), 'fenecdb-mcp-'));

/**
 * Where a fenec-server started on port 0 listens, off its `listening on:
 * http://<address>` line, read to the space after it so a line that came
 * in two pieces is not read short. A port found free and handed over was
 * free only until someone else took it, and a server of someone else's
 * could answer in its place.
 */
const listening = (child) =>
  new Promise((res, rej) => {
    let log = '';
    child.stderr.on('data', (d) => {
      log += d;
      const at = /listening on: (http:\/\/\S+)\s/.exec(log);
      if (at) res(at[1]);
    });
    child.on('exit', () => rej(new Error(`fenec-server ended before it listened: ${log}`)));
  });

const SEED = [
  'create collection docs (title text, body text @text, embed vector<3> @hnsw(cosine), year int)',
  'put docs [{title: "fox", body: "the quick brown fox jumps", embed: [1, 0, 0], year: 2023}, ' +
    '{title: "dog", body: "a lazy dog sleeps all day", embed: [0, 1, 0], year: 2024}, ' +
    '{title: "cat", body: "the cat watches the fox", embed: [0.9, 0.1, 0], year: 2025}]',
];

/** fenec-server with every tool on, over a file of its own, seeded. A
 * test's calls come quicker than an agent's: no rate. */
async function startServer() {
  const child = spawn(
    server,
    ['--http', '127.0.0.1:0', '--file', join(dir, 'server.fenec'), '--http-token', TOKEN,
      '--mcp-write', '--mcp-memory', 'memories', '--mcp-calls-per-sec', '0'],
    { stdio: ['ignore', 'ignore', 'pipe'] },
  );
  const url = await listening(child);
  for (const query of SEED) {
    const r = await fetch(`${url}/query`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${TOKEN}` },
      body: JSON.stringify({ query }),
    });
    assert.equal(r.status, 200, await r.text());
  }
  return { url, child };
}

const srv = await startServer();
after(async () => {
  srv.child.kill();
  await rm(dir, { recursive: true, force: true });
});

const ERAS = [
  ['legacy', undefined],
  ['2026-07-28 pinned', { mode: { pin: '2026-07-28' } }],
  ['auto', { mode: 'auto' }],
];

function client(negotiation) {
  return new Client(
    { name: 'fenecdb-integration', version: '1.0.0' },
    negotiation ? { versionNegotiation: negotiation } : {},
  );
}

/** Calls every tool through `c` and checks what each answers. */
async function everyTool(c, { memory = 'memories' } = {}) {
  const { tools } = await c.listTools();
  const names = tools.map((t) => t.name);
  assert.deepEqual(names, ['schema', 'search', 'query', 'get', 'write', 'remember', 'recall', 'forget']);
  for (const t of tools) {
    assert.equal(t.inputSchema.type, 'object', t.name);
    assert.equal(typeof t.description, 'string');
  }
  const call = async (name, args) => {
    const r = await c.callTool({ name, arguments: args });
    // The text a model reads is the structured content, written out.
    if (!r.isError) assert.deepEqual(JSON.parse(r.content[0].text), r.structuredContent);
    return r;
  };

  let r = await call('schema', {});
  assert.equal(r.isError, false);
  assert.match(r.structuredContent.fenecql, /create collection docs/);

  r = await call('search', { collection: 'docs', text: 'fox', vector: [1, 0, 0], limit: 2 });
  assert.equal(r.isError, false, JSON.stringify(r));
  assert.equal(r.structuredContent.rows[0].title, 'fox');
  assert.equal(r.structuredContent.rows[0].embed, undefined);
  assert.match(r.structuredContent.rows[0].snippet, /\*\*fox\*\*/);

  r = await call('query', { query: 'get docs where year >= $1 order year', params: [2024] });
  assert.deepEqual(r.structuredContent.rows.map((x) => x.title), ['dog', 'cat']);

  r = await call('get', { collection: 'docs', ids: [1, 404] });
  assert.equal(r.structuredContent.rows[0].title, 'fox');
  assert.deepEqual(r.structuredContent.missing, [404]);

  const title = `note ${Math.random()}`;
  r = await call('write', { statement: 'insert docs {title: $1, year: $2}', params: [title, 2026] });
  assert.deepEqual(r.structuredContent, { affected: 1 });
  r = await call('write', { statement: 'del docs where title = $1 require 1', params: [title] });
  assert.deepEqual(r.structuredContent, { affected: 1 });

  const fact = `the integration test ran at ${Date.now()}`;
  r = await call('remember', { text: fact, tags: ['ci'] });
  assert.deepEqual(r.structuredContent, { remembered: 1, collection: memory });
  r = await call('recall', { text: 'integration test', tags: ['ci'], limit: 10 });
  const found = r.structuredContent.memories.find((m) => m.text === fact);
  assert.ok(found, JSON.stringify(r.structuredContent));
  r = await call('forget', { id: found.id });
  assert.deepEqual(r.structuredContent, { forgotten: 1 });

  // A refusal is the tool's result, for the model to correct.
  r = await call('query', { query: 'del docs' });
  assert.equal(r.isError, true);
  assert.match(r.content[0].text, /query runs reads/);
}

for (const [era, negotiation] of ERAS) {
  test(`Streamable HTTP, ${era}`, async () => {
    const c = client(negotiation);
    const transport = new StreamableHTTPClientTransport(new URL(`${srv.url}/_mcp`), {
      requestInit: { headers: { Authorization: `Bearer ${TOKEN}` } },
    });
    await c.connect(transport);
    try {
      assert.equal(c.getServerVersion().name, 'fenecdb');
      await everyTool(c);
    } finally {
      await c.close();
    }
  });

  test(`stdio over a file, ${era}`, async () => {
    // A file of its own, made by the shell first.
    const file = join(dir, `stdio-${era.replace(/\W/g, '')}.fenec`);
    for (const q of SEED) {
      await new Promise((res, rej) => {
        const p = spawn(shell, [file, '-c', q], { stdio: 'ignore' });
        p.on('exit', (code) => (code === 0 ? res() : rej(new Error(`fenec -c exited ${code}`))));
      });
    }
    const c = client(negotiation);
    await c.connect(new StdioClientTransport({
      command: shell,
      args: ['mcp', '--file', file, '--write', '--memory', 'notes'],
      stderr: 'ignore',
    }));
    try {
      await everyTool(c, { memory: 'notes' });
    } finally {
      await c.close();
    }
  });

  test(`stdio bridged to the server, ${era}`, async () => {
    const c = client(negotiation);
    await c.connect(new StdioClientTransport({
      command: shell,
      args: ['mcp', '--url', `${srv.url}/_mcp`],
      env: { ...process.env, FENEC_TOKEN: TOKEN },
      stderr: 'ignore',
    }));
    try {
      await everyTool(c);
    } finally {
      await c.close();
    }
  });
}

/** The page's `<pre data-lang="json" data-label="...">` blocks that are a
 * client's configuration, by their label. */
async function configurations() {
  const page = await readFile(new URL('site/content/docs/mcp.html', root), 'utf8');
  const out = [];
  for (const m of page.matchAll(/<pre data-lang="json" data-label="([^"]+)">([\s\S]*?)<\/pre>/g)) {
    if (!/mcp\.json|claude_desktop_config\.json/.test(m[1])) continue;
    const text = m[2].replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
    out.push([m[1], JSON.parse(text)]);
  }
  return out;
}

const configs = await configurations();

test('the docs give a configuration for each client', () => {
  assert.deepEqual(configs.map(([label]) => label), [
    '.mcp.json',
    '.mcp.json, a server',
    '.cursor/mcp.json',
    '.cursor/mcp.json, a server',
    'claude_desktop_config.json',
  ]);
});

// What each client does with an entry: a variable expanded (Claude Code's
// `${VAR}`, Cursor's `${env:VAR}`), `fenec` found on the PATH, the file
// and the server's address this test's own.
const expand = (v) => v.replace(/\$\{(?:env:)?FENEC_TOKEN\}/g, TOKEN);
for (const [label, config] of configs) {
  test(`the configuration ${label} loads`, async () => {
    for (const [name, entry] of Object.entries(config.mcpServers)) {
      assert.equal(name, 'fenecdb');
      let transport;
      if (entry.url) {
        const url = new URL(entry.url);
        const headers = Object.fromEntries(Object.entries(entry.headers ?? {}).map(([k, v]) => [k, expand(v)]));
        transport = new StreamableHTTPClientTransport(new URL(url.pathname, srv.url), { requestInit: { headers } });
      } else {
        assert.equal(entry.command, 'fenec', label);
        const file = join(dir, `${label.replace(/\W/g, '')}.fenec`);
        // The server's address this test's own, the path the page's.
        const args = entry.args.map((a) =>
          a === 'notes.fenec' ? file : a.startsWith('http://') ? `${srv.url}${new URL(a).pathname}` : a,
        );
        const env = { ...process.env, ...Object.fromEntries(Object.entries(entry.env ?? {}).map(([k, v]) => [k, k === 'FENEC_TOKEN' ? TOKEN : v])) };
        transport = new StdioClientTransport({ command: shell, args, env, stderr: 'ignore' });
      }
      const c = client(undefined);
      await c.connect(transport);
      try {
        const { tools } = await c.listTools();
        assert.ok(tools.some((t) => t.name === 'recall' || t.name === 'search'), label);
        const r = await c.callTool({ name: 'schema', arguments: {} });
        assert.equal(r.isError, false, JSON.stringify(r));
      } finally {
        await c.close();
      }
    }
  });
}
