// Money is integer cents end to end: stored, summed and compared as whole
// numbers, and turned into text only here.
const fmt = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' });

export function money(cents: number): string {
  return fmt.format(cents / 100);
}

/** The price as schema.org wants it: a decimal string, "49.99". */
export function decimal(cents: number): string {
  return `${Math.floor(cents / 100)}.${String(cents % 100).padStart(2, '0')}`;
}

export const FREE_SHIPPING_FROM = 15000;
export const SHIPPING = 995;

export function shippingFor(subtotal: number): number {
  return subtotal === 0 || subtotal >= FREE_SHIPPING_FROM ? 0 : SHIPPING;
}
