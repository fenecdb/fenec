// A payment provider's capture, mocked. Like a real one (Stripe's
// Idempotency-Key, Adyen's reference), it is idempotent by a key: the same
// key captures once and answers with the same reference every time, so a
// retried checkout never charges twice. The card number decides the answer,
// as test cards do: one ending in 0002 is declined, 0069 has expired, any
// other is approved.
import { createHmac } from 'node:crypto';

const SECRET = process.env.SHOP_SECRET ?? 'shop-dev-session-secret-change-me';

export type Charge = { ok: true; ref: string } | { ok: false; reason: string };

const seen = new Map<string, Charge>();

export async function capture(req: { amount: number; card: string; key: string }): Promise<Charge> {
  const kept = seen.get(req.key);
  if (kept) return kept;
  const digits = req.card.replace(/\D/g, '');
  let charge: Charge;
  if (digits.length < 12 || digits.length > 19) charge = { ok: false, reason: 'card number incomplete' };
  else if (digits.endsWith('0002')) charge = { ok: false, reason: 'insufficient funds' };
  else if (digits.endsWith('0069')) charge = { ok: false, reason: 'card expired' };
  else charge = { ok: true, ref: `pay_${createHmac('sha256', SECRET).update(req.key).digest('hex').slice(0, 20)}` };
  if (seen.size > 50_000) seen.clear();
  seen.set(req.key, charge);
  return charge;
}
