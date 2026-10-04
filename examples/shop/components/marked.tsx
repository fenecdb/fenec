/**
 * Text with the words a search matched in <mark>, built from the offsets
 * `highlight()` and `snippet()` answer (UTF-16, a JavaScript string's own):
 * the text stays text, so nothing in a product's copy is read as HTML.
 */
export function Marked({ text, marks }: { text: string; marks: [number, number][] }) {
  const out: React.ReactNode[] = [];
  let at = 0;
  for (const [start, end] of marks) {
    const s = Math.max(start, at);
    if (end <= s) continue;
    if (s > at) out.push(text.slice(at, s));
    out.push(<mark key={s}>{text.slice(s, end)}</mark>);
    at = end;
  }
  if (at < text.length) out.push(text.slice(at));
  return <>{out}</>;
}
