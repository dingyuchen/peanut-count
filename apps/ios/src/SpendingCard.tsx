import { useMemo } from 'react';
import { Pressable, Text, View } from 'react-native';
import { breakdown, shares, shiftMonth, spendingIndex, trend } from './spending';
import type { Statement } from './storage';
import { Money, Segmented, formatCents, monthTitle, useTheme } from './ui';

const COLUMN_HEIGHT = 88;
const SHORT_MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const shortMonth = (month: string): string => SHORT_MONTHS[Number(month.slice(5, 7)) - 1];
const longMonth = (month: string): string => monthTitle(`${month}-01`).split(' ')[0];
const ratio = (value: bigint, max: bigint): number => (max > 0n && value > 0n ? Number((value * 10000n) / max) / 10000 : 0);

// Copilot/Penny-style spending card: a six-month column trend (tap to pick a month)
// above ranked horizontal bars for that month's expense categories. The selected month
// is owned by the home screen so the transaction list below follows it.
export default function SpendingCard({ statements, month, first, last, onMonthChange, currency: chosen, onCurrencyChange }: {
  statements: Statement[];
  month: string | undefined;
  first: string | undefined;
  last: string | undefined;
  onMonthChange: (month: string) => void;
  currency: string | null;
  onCurrencyChange: (currency: string) => void;
}) {
  const { c, s } = useTheme();
  const index = useMemo(() => spendingIndex(statements), [statements]);
  const currency = chosen && index.currencies.includes(chosen) ? chosen : index.currencies[0];
  if (!currency || !month || !first || !last) return null;

  const ledger = index.byCurrency[currency];
  const windowEnd = month > shiftMonth(last, -6) ? last : month;
  const columns = trend(ledger, windowEnd);
  const columnMax = columns.reduce((max, { total }) => (total > max ? total : max), 0n);
  const total = ledger.months[month]?.total ?? 0n;
  const previous = ledger.months[shiftMonth(month, -1)]?.total ?? 0n;
  const categories = breakdown(ledger, month);
  const categoryMax = categories.reduce((max, { value }) => (value > max ? value : max), 0n);
  const percents = shares(categories.map(({ value }) => value));

  let comparison = 'No spending recorded the month before';
  if (previous > 0n) {
    const change = Math.round(Number(((total - previous) * 1000n) / previous) / 10);
    comparison = change === 0 ? `Same as ${longMonth(shiftMonth(month, -1))}`
      : `${Math.abs(change)}% ${change > 0 ? 'more' : 'less'} than ${longMonth(shiftMonth(month, -1))}`;
  }

  const canGoBack = month > first;
  const canGoForward = month < last;
  const Arrow = ({ label, glyph, enabled, delta }: { label: string; glyph: string; enabled: boolean; delta: number }) => <Pressable accessibilityRole="button" accessibilityLabel={label}
    hitSlop={10} disabled={!enabled} onPress={() => onMonthChange(shiftMonth(month, delta))}
    style={({ pressed }) => [{ paddingHorizontal: 8 }, pressed && s.pressed, !enabled && s.disabled]}>
    <Text style={[s.headline, { color: c.accent }]}>{glyph}</Text>
  </Pressable>;

  return <View style={s.card}>
    <View style={[s.row, { justifyContent: 'space-between' }]}>
      <Text style={s.eyebrow} accessibilityRole="header">Spending</Text>
      <View style={[s.row, { gap: 4 }]}>
        <Arrow label="Previous month" glyph="‹" enabled={canGoBack} delta={-1} />
        <Text style={[s.headline, { minWidth: 120, textAlign: 'center' }]}>{monthTitle(`${month}-01`)}</Text>
        <Arrow label="Next month" glyph="›" enabled={canGoForward} delta={1} />
      </View>
    </View>

    {index.currencies.length > 1 && <Segmented value={currency} onChange={onCurrencyChange}
      options={index.currencies.slice(0, 4).map((code) => ({ value: code, label: code }))} />}

    <View style={{ gap: 2 }}>
      <View style={[s.row, { alignItems: 'flex-end', gap: 6 }]}>
        <Money cents={total} size={34} weight="800" unit={currency} />
        <Text style={[s.secondary, { marginBottom: 6 }]}>{currency}</Text>
      </View>
      <Text style={s.footnote}>{comparison}</Text>
    </View>

    <View style={{ flexDirection: 'row', alignItems: 'flex-end', height: COLUMN_HEIGHT + 24, borderBottomWidth: 1, borderBottomColor: c.border }}>
      {columns.map((column) => {
        const selected = column.month === month;
        const height = column.total > 0n ? Math.max(3, Math.round(ratio(column.total, columnMax) * COLUMN_HEIGHT)) : 0;
        return <Pressable key={column.month} onPress={() => onMonthChange(column.month)}
          accessibilityRole="button" accessibilityState={{ selected }}
          accessibilityLabel={`${monthTitle(`${column.month}-01`)}: ${formatCents(column.total)} ${currency} spent`}
          style={{ flex: 1, alignItems: 'center', justifyContent: 'flex-end', height: '100%' }}>
          {selected && column.total > 0n && <View style={{ marginBottom: 4 }}>
            <Money cents={column.total} size={13} weight="600" />
          </View>}
          <View style={{
            width: 24, height, backgroundColor: selected ? c.accent : c.chartMuted,
            borderTopLeftRadius: 4, borderTopRightRadius: 4,
          }} />
        </Pressable>;
      })}
    </View>
    <View style={{ flexDirection: 'row', marginTop: -6 }}>
      {columns.map((column) => <Text key={column.month} style={[s.footnote, { flex: 1, textAlign: 'center' },
        column.month === month && { color: c.text, fontWeight: '600' }]}>{shortMonth(column.month)}</Text>)}
    </View>

    <View style={s.divider} />

    {categories.length === 0
      ? <Text style={s.secondary}>No expenses in {monthTitle(`${month}-01`)}.</Text>
      : <View style={{ gap: 14 }}>
        {categories.map((category, position) => <View key={category.name} style={{ gap: 6 }} accessible
          accessibilityLabel={`${category.name}: ${formatCents(category.value)} ${currency}, ${percents[position]} percent`}>
          <View style={[s.row, { justifyContent: 'space-between' }]}>
            <Text style={[s.body, { flex: 1 }, category.other && { color: c.muted }]} numberOfLines={1}>{category.name}</Text>
            <Text style={[s.body, { fontVariant: ['tabular-nums'], fontWeight: '600' }]}>{formatCents(category.value)}</Text>
            <Text style={[s.footnote, { width: 44, textAlign: 'right', fontVariant: ['tabular-nums'] }]} numberOfLines={1}>{percents[position]}%</Text>
          </View>
          <View style={{
            height: 8, width: `${Math.max(1, ratio(category.value, categoryMax) * 100)}%`,
            backgroundColor: category.other ? c.chartMuted : c.accent,
            borderTopRightRadius: 4, borderBottomRightRadius: 4,
          }} />
        </View>)}
      </View>}
  </View>;
}
