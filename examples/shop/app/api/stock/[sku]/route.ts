import { stockOf } from '../../../../lib/catalog';
import { json } from '../../../../lib/http';

export async function GET(_req: Request, { params }: { params: Promise<{ sku: string }> }) {
  const { sku } = await params;
  if (!/^SG-\d{6}$/.test(sku)) return json({ error: 'no such product' }, 404);
  return json({ sku, available: await stockOf(sku) });
}
