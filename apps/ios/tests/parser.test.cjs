const test = require('node:test');
const assert = require('node:assert/strict');
const { parseStatement } = require('../src/parser.cjs');

test('keeps exact amounts and suggests counter-accounts for signed cash flows', () => {
  const statement = parseStatement([
    'Date Description Amount',
    '2026-04-01 Opening Balance +1,000.00',
    '2026-04-06 Card purchase Coffee Corner -4.75 USD',
    '2026-04-20 Payroll deposit +1,250.00 USD',
    '21/04/2026 Groceries (21.50) USD',
    '22/04/2026 Rail ticket 2.00 DR USD',
  ].join('\n'), 'USD', 'DMY');
  assert.deepEqual(statement, {
    skipped: 0,
    rows: [
      { date: '2026-04-06', description: 'Card purchase Coffee Corner', amount: '-4.75', account: 'Expenses:Dining' },
      { date: '2026-04-20', description: 'Payroll deposit', amount: '1250.00', account: 'Income:Salary' },
      { date: '2026-04-21', description: 'Groceries', amount: '-21.50', account: 'Expenses:Groceries' },
      { date: '2026-04-22', description: 'Rail ticket', amount: '-2.00', account: 'Expenses:Transport' },
    ],
  });
});

test('only reads signed amounts from running-balance layouts with an explicit header', () => {
  const statement = parseStatement([
    'Date Description Amount Balance',
    '2026-04-06 Coffee -4.75 1,045.25',
    '2026-04-06 Unknown 19.00 1,064.25',
    '2026-04-31 Invalid date -4.75 1,040.50',
    '2026-04-28 Grocery -4.75 EUR 100.00',
  ].join('\n'), 'USD', 'MDY');
  assert.equal(statement.skipped, 3);
  assert.deepEqual(statement.rows.map(({ amount }) => amount), ['-4.75']);
});

test('detects the statement date format', () => {
  const { detectDateOrder } = require('../src/parser.cjs');
  assert.equal(detectDateOrder('2026-04-01 A -1.00\n2026-04-02 B -1.00'), 'ISO');
  assert.equal(detectDateOrder('21/04/2026 A -1.00\n02/05/2026 B -1.00'), 'DMY');
  assert.equal(detectDateOrder('04/21/2026 A -1.00\n05/02/2026 B -1.00'), 'MDY');
  assert.equal(detectDateOrder('04/05/2026 A -1.00'), null);
  assert.equal(detectDateOrder('21/04/2026 A -1.00\n04/21/2026 B -1.00'), null);
  assert.equal(detectDateOrder('no dates here'), null);
});
