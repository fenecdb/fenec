// Money as integers of minor units. Every currency here has two decimals;
// one with none or three would carry its exponent beside its code.

/** "1234.5" -> 123450; null when it is not an amount. */
export function parseAmount(text: string): number | null {
  const m = /^\s*(\d{1,12})(?:[.,](\d{1,2}))?\s*$/.exec(text);
  if (!m) return null;
  const n = Number(m[1]) * 100 + Number((m[2] ?? '').padEnd(2, '0'));
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

/** 123450 -> "1,234.50"; a negative in parentheses, as a ledger writes it. */
export function formatAmount(minor: number): string {
  const abs = Math.abs(minor);
  const whole = Math.floor(abs / 100).toLocaleString('en-GB');
  const s = `${whole}.${String(abs % 100).padStart(2, '0')}`;
  return minor < 0 ? `(${s})` : s;
}

const SYMBOL: Record<string, string> = { EUR: '€', GBP: '£', USD: '$' };
export const symbol = (currency: string) => SYMBOL[currency] ?? `${currency} `;
