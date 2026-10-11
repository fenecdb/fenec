#!/usr/bin/env node
// An agent whose memory outlives its conversations: Claude, given
// fenecdb's MCP tools over stdio -- `fenec mcp --file agent.fenec --memory
// memories` -- recalls what it was told in an earlier run before it
// answers, and remembers what is worth keeping.
//
//   node agent.mjs "I indent with tabs, and the deploy runs on Fridays"
//   node agent.mjs "when does the deploy run?"
//
// The tools come from the server (tools/list) and are handed to the
// Anthropic SDK's tool runner as they are (`mcpTools`), so a tool the
// server adds needs no code here. No embedding model is needed: memories
// are recalled by their words (BM25), and by meaning too once `remember`
// and `recall` are handed vectors. `--check` runs the tools without the
// model, which CI does: no API key there.

import Anthropic from '@anthropic-ai/sdk';
import { mcpTools } from '@anthropic-ai/sdk/helpers/beta/mcp';
import { Client } from '@modelcontextprotocol/client';
import { StdioClientTransport } from '@modelcontextprotocol/client/stdio';

const args = process.argv.slice(2);
const check = args.includes('--check');
const question = args.filter((a) => a !== '--check').join(' ').trim();
if (!check && !question) {
  console.error('usage: node agent.mjs "<what to tell or ask the agent>" | --check');
  process.exit(2);
}

const mcp = new Client({ name: 'fenecdb-example-agent', version: '1.0.0' });
await mcp.connect(
  new StdioClientTransport({
    command: process.env.FENEC_BIN ?? 'fenec',
    args: ['mcp', '--file', process.env.FENEC_FILE ?? 'agent.fenec', '--memory', 'memories'],
    stderr: 'ignore',
  }),
);

try {
  const { tools } = await mcp.listTools();
  const runnable = mcpTools(tools, mcp);

  if (check) {
    // The tools as the runner would call them, without the model.
    const tool = (name) => runnable.find((t) => t.name === name);
    await tool('remember').run({ text: 'the example agent ran its check', tags: ['check'] });
    const recalled = await tool('recall').run({ text: 'example agent check', tags: ['check'] });
    const text = typeof recalled === 'string' ? recalled : JSON.stringify(recalled);
    if (!text.includes('the example agent ran its check')) {
      throw new Error(`recall did not find the memory: ${text}`);
    }
    console.log(`ok: ${runnable.map((t) => t.name).join(', ')}`);
  } else {
    const anthropic = new Anthropic();
    const message = await anthropic.beta.messages.toolRunner({
      model: 'claude-opus-5-5',
      max_tokens: 16000,
      output_config: { effort: 'medium' },
      // Should a safety classifier decline the request, the API answers
      // it with the model it routes that category to.
      betas: ['server-side-fallback-2026-07-01'],
      fallbacks: 'default',
      system:
        `${mcp.getInstructions() ?? ''}\n\n` +
        'You are a helpful assistant with a memory that lasts between conversations. ' +
        'Before answering, recall what you may already know about the subject. ' +
        'When the user tells you a fact, a preference or a decision worth keeping, remember it ' +
        'as one sentence that stands on its own.',
      tools: runnable,
      messages: [{ role: 'user', content: question }],
    });
    if (message.stop_reason === 'refusal') {
      console.error('The request was declined.');
      process.exitCode = 1;
    }
    for (const block of message.content) {
      if (block.type === 'text') console.log(block.text);
    }
  }
} finally {
  await mcp.close();
}
