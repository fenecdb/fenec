import wasm from '@fenecdb/web/fenec.wasm?url';
import { open, add, finish, query } from './notes.js';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const list = $<HTMLUListElement>('list');
const words = $<HTMLInputElement>('words');
const tag = $<HTMLInputElement>('tag');
const onlyOpen = $<HTMLInputElement>('open');

const db = await open(wasm);

// A live query: the rows now, and again after every write to `notes`.
let stop = () => {};
function watch() {
  stop();
  stop = db.live(query(db, { words: words.value, tag: tag.value.trim(), open: onlyOpen.checked }), (rows) => {
    list.replaceChildren(
      ...rows.map((n) => {
        const li = document.createElement('li');
        li.className = n.done ? 'done' : '';
        li.innerHTML = `<b></b> <span class="tags"></span><p></p>`;
        li.querySelector('b')!.textContent = n.title ?? '';
        li.querySelector('.tags')!.textContent = (n.tags ?? []).map((t) => `#${t}`).join(' ');
        li.querySelector('p')!.textContent = n.body ?? '';
        li.onclick = () => finish(db, n.id);
        return li;
      }),
    );
  });
}
for (const el of [words, tag, onlyOpen]) el.addEventListener('input', watch);
watch();

$<HTMLFormElement>('add').addEventListener('submit', async (e) => {
  e.preventDefault();
  const form = e.target as HTMLFormElement;
  const data = new FormData(form);
  const tags = String(data.get('tags')).split(',').map((t) => t.trim()).filter(Boolean);
  await add(db, String(data.get('title')), String(data.get('body')), tags);
  form.reset();
});
