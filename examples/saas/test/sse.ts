// Server-sent events off a fetch body. @fenecdb/web/client keeps its own
// reader to itself (README, gaps), so the tests carry this one, as
// public/app.js does.
export async function* sseEvents(res: Response): AsyncGenerator<{ name: string; data: string }> {
  const reader = res.body!.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      buf += decoder.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf('\n\n')) >= 0) {
        const block = buf.slice(0, i);
        buf = buf.slice(i + 2);
        let name = '';
        let data = '';
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) name = line.slice(6).trim();
          else if (line.startsWith('data:')) data += line.slice(5).trim();
        }
        if (name) yield { name, data };
      }
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
}
