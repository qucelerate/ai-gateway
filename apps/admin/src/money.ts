// Amounts from the reporting API are "money units": 10⁻⁶ of the currency's
// smallest unit (a micro-cent for USD), so sub-cent inference costs stay exact.
const MONEY_UNIT_DIGITS = 6;

/** Digits of the currency's smallest unit (2 for USD, 0 for JPY). */
export function minorDigits(currency: string): number {
  try {
    return new Intl.NumberFormat('en', { style: 'currency', currency }).resolvedOptions().maximumFractionDigits ?? 2;
  } catch {
    return 2;
  }
}

/** A money-unit amount as a currency string. Amounts under one major unit
 * show every money-unit digit, so no charge ever reads as zero. */
export function formatMoney(amount: number | null | undefined, currency: string): string {
  if (amount === null || amount === undefined) return '—';
  const digits = minorDigits(currency);
  const major = amount / 10 ** (MONEY_UNIT_DIGITS + digits);
  const small = major !== 0 && Math.abs(major) < 1;
  try {
    return new Intl.NumberFormat(undefined, {
      style: 'currency',
      currency,
      minimumFractionDigits: digits,
      maximumFractionDigits: small ? digits + MONEY_UNIT_DIGITS : digits,
    }).format(major);
  } catch {
    return `${major.toFixed(digits)} ${currency}`;
  }
}

/** A typed amount in major units ("12.50") as money units, or null if it is not one. */
export function parseMoney(value: string, currency: string): number | null {
  const match = /^\s*(\d+)(?:\.(\d*))?\s*$/.exec(value);
  if (!match) return null;
  const scale = MONEY_UNIT_DIGITS + minorDigits(currency);
  const fraction = match[2] ?? '';
  if (fraction.length > scale) return null;
  const units = Number(match[1]) * 10 ** scale + Number(fraction.padEnd(scale, '0') || '0');
  return Number.isSafeInteger(units) ? units : null;
}

/** A money-unit amount in major units as a plain decimal ("12.5",
 * "0.00000001"), exactly, never in scientific notation: what a form field
 * shows so that saving it unchanged sends the same amount. */
export function toMajor(amount: number, currency: string): string {
  const scale = MONEY_UNIT_DIGITS + minorDigits(currency);
  const digits = String(Math.abs(Math.trunc(amount))).padStart(scale + 1, '0');
  const whole = digits.slice(0, -scale);
  const fraction = digits.slice(-scale).replace(/0+$/, '');
  return `${amount < 0 ? '-' : ''}${whole}${fraction ? `.${fraction}` : ''}`;
}

export function formatCount(value: number | null | undefined): string {
  return value === null || value === undefined ? '—' : value.toLocaleString();
}
