import test from 'node:test';
import assert from 'node:assert/strict';
import { breakdown, latestMonth, shares, shiftMonth, spendingIndex, trend, type SpendingRow } from '../src/spending.ts';

const statement = (currency: string, rows: [string, string, string][]): { currency: string; rows: SpendingRow[] } =>
  ({ currency, rows: rows.map(([date, amount, account]) => ({ date, amount, account })) });

test('sums exact cents per month and category, netting refunds and ignoring income', () => {
  const index = spendingIndex([
    statement('USD', [
      ['2026-11-02', '-4.75', 'Expenses:Dining'],
      ['2026-11-03', '-0.10', 'Expenses:Dining'],
      ['2026-11-04', '-0.20', 'Expenses:Dining'],
      ['2026-11-05', '2.00', 'Expenses:Dining'],
      ['2026-11-06', '1500.00', 'Income:Salary'],
      ['2026-10-30', '-21.50', 'Expenses:Groceries'],
    ]),
    statement('EUR', [['2026-11-01', '-9.99', 'Expenses:Transport']]),
  ]);
  assert.deepEqual(index.currencies, ['USD', 'EUR']);
  const usd = index.byCurrency.USD;
  assert.equal(latestMonth(usd), '2026-11');
  assert.equal(usd.months['2026-11'].total, 305n);
  assert.deepEqual(breakdown(usd, '2026-11'), [{ name: 'Dining', value: 305n }]);
  assert.deepEqual(trend(usd, '2026-11').map(({ month, total }) => [month, total]), [
    ['2026-06', 0n], ['2026-07', 0n], ['2026-08', 0n], ['2026-09', 0n], ['2026-10', 2150n], ['2026-11', 305n],
  ]);
});

test('ranks categories and folds the tail into Other', () => {
  const rows: [string, string, string][] = ['A', 'B', 'C', 'D', 'E', 'F', 'G'].map((name, index) => ['2026-01-05', `-${index + 1}.00`, `Expenses:${name}`]);
  const result = breakdown(spendingIndex([statement('USD', rows)]).byCurrency.USD, '2026-01');
  assert.deepEqual(result.map(({ name, value }) => [name, value]), [
    ['G', 700n], ['F', 600n], ['E', 500n], ['D', 400n], ['C', 300n], ['Other', 300n],
  ]);
});

test('shifts months across year boundaries', () => {
  assert.equal(shiftMonth('2026-01', -1), '2025-12');
  assert.equal(shiftMonth('2025-12', 1), '2026-01');
  assert.equal(shiftMonth('2026-03', -14), '2025-01');
});

test('shares add up to exactly 100 percent', () => {
  assert.deepEqual(shares([10230n, 9600n, 7135n, 1599n]), [36, 34, 25, 5]);
  assert.deepEqual(shares([1n, 1n, 1n]), [34, 33, 33]);
  assert.deepEqual(shares([0n, 0n]), [0, 0]);
});

test('months with only income still appear in the index', () => {
  const index = spendingIndex([statement('USD', [['2026-12-01', '100.00', 'Income:Salary'], ['2026-11-01', '-1.00', 'Expenses:A']])]);
  assert.equal(latestMonth(index.byCurrency.USD), '2026-12');
  assert.equal(index.byCurrency.USD.months['2026-12'].total, 0n);
});
