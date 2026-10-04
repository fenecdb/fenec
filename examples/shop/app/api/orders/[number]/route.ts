import { orderOf } from '../../../../lib/orders';
import { shopper } from '../../../../lib/session';
import { json } from '../../../../lib/http';

export async function GET(_req: Request, { params }: { params: Promise<{ number: string }> }) {
  const { number } = await params;
  const s = await shopper();
  const o = s ? await orderOf(s.owner, number) : null;
  return o ? json(o) : json({ error: 'no such order' }, 404);
}
