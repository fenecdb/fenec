// A token for the browser's live stock: role `stock`, which policy.txt
// lets read `inventory` and nothing else, for ten minutes.
import { tokenFor } from '../../../lib/db';
import { json } from '../../../lib/http';

const PUBLIC_URL = process.env.FENEC_PUBLIC_URL ?? process.env.FENEC_URL ?? 'http://127.0.0.1:8080';

export async function GET() {
  return json({ url: PUBLIC_URL, token: tokenFor('stock-viewer', 'stock') });
}
