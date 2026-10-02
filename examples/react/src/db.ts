import wasm from '@fenecdb/web/fenec.wasm?url';
import { local, synced } from './open.js';

export const SERVER = import.meta.env.VITE_FENEC_URL ?? 'http://127.0.0.1:8080';
export const TOKEN = import.meta.env.VITE_FENEC_TOKEN ?? 'secret'; // a dev default; a real app hands each user a JWT

// The one line: a database in the page alone, or the same app synced with a
// fenec-server (started with --http-cors http://localhost:5173).
export const db = await local(wasm);
// export const db = await synced(SERVER, TOKEN, wasm);
