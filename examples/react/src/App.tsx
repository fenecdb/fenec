import { useState, type FormEvent } from 'react';
import { useLiveQuery } from '@fenecdb/react';
import { db } from './db.js';
import { notes, embed } from './tables.js';

export function App() {
  const [words, setWords] = useState('');
  const [tag, setTag] = useState('');
  const [open, setOpen] = useState(false);

  // Built again on every render; useLiveQuery subscribes again only when it
  // asks something else, and renders again after every write to notes.
  let q = db.from(notes).select('id', 'title', 'body', 'tags', 'done');
  if (tag) q = q.where('tags', 'has', tag);
  if (open) q = q.where('done', false);
  const rows = useLiveQuery(
    words.trim() ? q.match('body', words).near('embed', embed(words)).fuse().limit(20) : q.order('at', 'desc').limit(20),
  );

  async function add(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const form = e.currentTarget;
    const data = new FormData(form);
    const [title, body] = [String(data.get('title')), String(data.get('body'))];
    const tags = String(data.get('tags')).split(',').map((t) => t.trim()).filter(Boolean);
    form.reset();
    await db.from(notes).insert({ title, body, tags, done: false, at: new Date(), embed: embed(`${title} ${body}`) });
  }

  return (
    <main>
      <h1>Notes</h1>
      <form onSubmit={add}>
        <input name="title" placeholder="Title" required />
        <input name="body" placeholder="What about it" required />
        <input name="tags" placeholder="tags, comma separated" />
        <button>Add</button>
      </form>
      <div className="filters">
        <input value={words} onChange={(e) => setWords(e.target.value)} placeholder="Search" />
        <input value={tag} onChange={(e) => setTag(e.target.value.trim())} placeholder="tag" />
        <label>
          <input type="checkbox" checked={open} onChange={(e) => setOpen(e.target.checked)} /> open only
        </label>
      </div>
      {rows === undefined ? (
        <p>Loading…</p>
      ) : (
        <ul>
          {rows.map((n) => (
            <li key={n.id} className={n.done ? 'done' : ''} onClick={() => db.from(notes).where('id', n.id).update({ done: true })}>
              <b>{n.title}</b> <span className="tags">{(n.tags ?? []).map((t) => `#${t}`).join(' ')}</span>
              <p>{n.body}</p>
            </li>
          ))}
        </ul>
      )}
    </main>
  );
}
