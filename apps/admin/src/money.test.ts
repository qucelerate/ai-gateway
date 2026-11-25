import { describe, expect, it } from 'vitest';
import { formatMoney, minorDigits, parseMoney, toMajor } from './money';

describe('money units', () => {
  it('knows each currency’s minor digits', () => {
    expect(minorDigits('USD')).toBe(2);
    expect(minorDigits('JPY')).toBe(0);
  });

  it('formats money units as currency, keeping sub-cent amounts visible', () => {
    expect(formatMoney(1_250_000_000, 'USD')).toContain('12.50');
    expect(formatMoney(30_000, 'USD')).toContain('0.0003');
    expect(formatMoney(1, 'USD')).toContain('0.00000001');
    expect(formatMoney(0, 'USD')).toContain('0.00');
    expect(formatMoney(null, 'USD')).toBe('—');
  });

  it('parses major units exactly', () => {
    expect(parseMoney('12.5', 'USD')).toBe(1_250_000_000);
    expect(parseMoney('0', 'USD')).toBe(0);
    expect(parseMoney('0.0003', 'USD')).toBe(30_000);
    expect(parseMoney('3', 'JPY')).toBe(3_000_000);
    expect(parseMoney('-1', 'USD')).toBeNull();
    expect(parseMoney('1.2.3', 'USD')).toBeNull();
    expect(parseMoney('0.000000001', 'USD')).toBeNull();
    expect(parseMoney('', 'USD')).toBeNull();
  });

  it('shows amounts as plain decimals that parse back exactly', () => {
    for (const [amount, currency, text] of [[1, 'USD', '0.00000001'], [10_000_000_000, 'USD', '100'], [1_250_000_000, 'USD', '12.5'], [3_000_000, 'JPY', '3'], [0, 'USD', '0']] as const) {
      expect(toMajor(amount, currency)).toBe(text);
      expect(parseMoney(text, currency)).toBe(amount);
    }
  });
});
