// A fresh five-minute token for the page's own requests to the router:
// the live map's streams, a customer's tracking. Held to the person's role
// by the nodes, whatever the page does with it.
import { json } from '../../../lib/http';
import { browserAccess, current } from '../../../lib/session';

export async function GET() {
  const a = await current();
  if (!a) return json({ error: 'sign in first' }, 401);
  return json(browserAccess(a));
}
