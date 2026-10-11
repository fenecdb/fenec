# An agent with a memory

Not the Notes app: Claude, with fenecdb's MCP tools, remembering across
runs. `agent.mjs` starts `fenec mcp --file agent.fenec --memory memories`,
hands the server's tools to the Anthropic SDK's tool runner as they are
(`mcpTools`), and asks Claude to recall before it answers and to remember
what is worth keeping.

```sh
npm install
export ANTHROPIC_API_KEY=...        # or `ant auth login`
node agent.mjs "I indent with tabs, and the deploy runs on Fridays"
node agent.mjs "when does the deploy run?"
```

It needs `fenec` on the `PATH` (`cargo install --path crates/fenec-cli`, or
`FENEC_BIN=../../target/debug/fenec`). No embedding model: memories are
recalled by their words, BM25 over an `@text` field, and by meaning too once
the tools are handed vectors. `node agent.mjs --check` runs the tools without
the model, as CI does (`make mcp-test`).

The ten lines that matter:

```js
const mcp = new Client({ name: 'fenecdb-example-agent', version: '1.0.0' });
await mcp.connect(new StdioClientTransport({
  command: 'fenec', args: ['mcp', '--file', 'agent.fenec', '--memory', 'memories'],
}));
const { tools } = await mcp.listTools();
const message = await new Anthropic().beta.messages.toolRunner({
  model: 'claude-opus-5-5', max_tokens: 16000,
  system: mcp.getInstructions(),
  tools: mcpTools(tools, mcp),
  messages: [{ role: 'user', content: question }],
});
```

`.mcp.json` here is the same server for Claude Code: open this folder in it
and the tools are there. See [Agents and MCP](https://fenecdb.com/docs/mcp).
