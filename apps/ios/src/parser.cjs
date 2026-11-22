const accountPattern = /^(Assets|Liabilities|Income|Expenses|Equity)(:[A-Z0-9][A-Za-z0-9-]*)+$/;
const validAccount = (account) => account.length <= 255 && accountPattern.test(account);
const currencyPattern = /^[A-Z]{3}$/;
const dateStart = /^(\d{4}-\d{2}-\d{2}|\d{1,2}[/.]\d{1,2}[/.]\d{4})\s+/;
const amountPattern = /^(?<sign>[+-]?)(?<whole>(?:\d{1,3}(?:,\d{3})+|\d+))\.(?<fraction>\d{2})$/;

function readDate(value, dateOrder) {
  let date = value;
  if (!value.includes('-')) {
    const [first, second, year] = value.split(/[/.]/);
    const [month, day] = dateOrder === 'DMY' ? [second, first] : [first, second];
    date = `${year}-${month.padStart(2, '0')}-${day.padStart(2, '0')}`;
  }
  const parsed = new Date(`${date}T00:00:00Z`);
  if (!Number.isFinite(parsed.getTime()) || parsed.toISOString().slice(0, 10) !== date || date < '1900-01-01') {
    return null;
  }
  return date;
}

function readAmount(value, marker) {
  const parens = value.startsWith('(') && value.endsWith(')');
  const cleaned = (parens ? value.slice(1, -1) : value).replace(/^(?<sign>[+-]?)[$£€]/, '$<sign>');
  const parts = cleaned.match(amountPattern)?.groups;
  if (!parts || (parens && parts.sign) || (marker && (parts.sign || parens))) return null;
  const whole = parts.whole.replace(/,/g, '').replace(/^0+(?=\d)/, '');
  if (whole.length > 18 || (whole === '0' && parts.fraction === '00')) return null;
  if (!parts.sign && !parens && !marker) return null;
  return `${parts.sign === '-' || parens || marker === 'DR' ? '-' : ''}${whole}.${parts.fraction}`;
}

function category(description, amount) {
  if (!amount.startsWith('-')) {
    return /\b(payroll|salary|wages)\b/i.test(description)
      ? 'Income:Salary' : 'Income:Uncategorized';
  }
  if (/\b(grocery|groceries|supermarket)\b/i.test(description)) return 'Expenses:Groceries';
  if (/\b(coffee|cafe|restaurant|dining)\b/i.test(description)) return 'Expenses:Dining';
  if (/\b(rail|transit|taxi|parking)\b/i.test(description)) return 'Expenses:Transport';
  return 'Expenses:Uncategorized';
}

function parseStatement(text, currency, dateOrder) {
  if (!currencyPattern.test(currency)) throw new Error('Enter a three-letter statement currency.');
  const rows = [];
  let skipped = 0;
  let hasRunningBalance = false;
  for (const original of text.split(/\r?\n/)) {
    const line = original.trim().replace(/[−–]/g, '-');
    const dateMatch = line.match(dateStart);
    if (!dateMatch) {
      if (/^date\s+.+\s+amount\s+balance$/i.test(line)) hasRunningBalance = true;
      else if (/^date\s+.+\s+amount$/i.test(line)) hasRunningBalance = false;
      continue;
    }

    const date = readDate(dateMatch[1], dateOrder);
    const fields = line.slice(dateMatch[0].length).trim().split(/\s+/);
    if (/^(opening balance|closing balance|balance brought forward|balance carried forward|previous balance)\b/i.test(fields.join(' '))) continue;
    if (hasRunningBalance) {
      const balance = fields.pop()?.replace(/^[+-]?[$£€]?/, '').replace(/[()]/g, '');
      if (!balance || !amountPattern.test(balance)) {
        skipped++;
        continue;
      }
    }
    let marker;
    let code;
    for (let index = 0; index < 2; index++) {
      const tail = fields.at(-1) ?? '';
      if (!marker && /^(CR|DR)$/i.test(tail)) marker = fields.pop().toUpperCase();
      else if (!code && /^[A-Z]{3}$/.test(tail)) code = fields.pop();
      else break;
    }
    const amountToken = fields.pop() ?? '';
    const symbol = amountToken.match(/[$£€]/)?.[0];
    const symbolMatches = !symbol || {
      '$': ['USD', 'CAD', 'AUD', 'NZD', 'SGD', 'HKD'],
      '£': ['GBP'], '€': ['EUR']
    }[symbol].includes(currency);
    const amount = readAmount(amountToken, marker);
    const description = fields.join(' ');
    if (!date || !amount || !symbolMatches || (code && code !== currency) || !description
        || encodeURIComponent(description).replace(/%[0-9A-F]{2}/g, 'x').length > 512 || /[\u0000-\u001f\u007f]/.test(description)
        || /(?:^|\s)[+-]?(?:\$|£|€)?\d[\d,]*\.\d{2}(?=\s|$)/.test(description)) {
      skipped++;
      continue;
    }
    rows.push({ date, description, amount, account: category(description, amount) });
  }
  return { rows, skipped };
}

module.exports = { validAccount, currencyPattern, parseStatement };
