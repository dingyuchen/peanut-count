import { useEffect, useMemo, useRef, useState, type Dispatch, type ReactElement, type ReactNode, type SetStateAction } from 'react';
import {
  ActivityIndicator, Alert, FlatList, KeyboardAvoidingView, Linking, Modal, RefreshControl, ScrollView,
  SectionList, StatusBar, StyleSheet, Switch, Text, View,
} from 'react-native';
import { SafeAreaProvider, SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';
import { requireNativeModule } from 'expo';
import * as DocumentPicker from 'expo-document-picker';
import { useIncomingShare } from 'expo-sharing';
import { account, clearProfile, storedProfile, syncStatements, type AuthMode, type Profile } from './api';
import { currencyPattern, detectDateOrder, parseStatement, validAccount, type DateOrder } from './parser';
import SpendingCard from './SpendingCard';
import {
  LockedLedgerError, addStatement, conflictCount, eraseLocalLedger, loadMode, resolveConflicts, saveMode, statements,
  type LedgerMode, type Statement, type StatementRow,
} from './storage';
import {
  Banner, Button, Chips, Field, NavBar, Pill, Segmented, formatAmount, formatCents, monthTitle, shortDate, toCents, useTheme,
  type BannerTone,
} from './ui';

const pdf = requireNativeModule<{ read(uri: string): Promise<{ id: string; text: string }> }>('StatementPdf');

const LOCAL_SCOPE = 'device';
const isStatementAccount = (value: string): boolean => validAccount(value) && /^(Assets|Liabilities):/.test(value);
const plural = (count: number, word: string): string => `${count} ${word}${count === 1 ? '' : 's'}`;

type Phase = 'loading' | 'locked' | 'ready';
type Busy = 'import' | 'post' | 'sync' | 'mode' | 'auth' | 'signout' | null;
type Notice = { tone: BannerTone; text: string } | null;

interface DraftRow extends StatementRow {
  excluded?: boolean;
}

// A statement being imported: the PDF's text, the details entered on the setup
// screen, and (after "Find transactions") the parsed rows under review.
interface Draft {
  id: string;
  text: string;
  source: string;
  currency: string;
  order: DateOrder;
  detected: DateOrder | 'ISO' | null;
  rows: DraftRow[] | null;
  skipped: number;
}

interface ReviewDraft extends Draft {
  rows: DraftRow[];
}

type SetDraft = Dispatch<SetStateAction<Draft | null>>;

const message = (error: unknown): string => (error instanceof Error ? error.message : String(error));

function BottomBar({ children }: { children: ReactNode }) {
  const { s } = useTheme();
  const insets = useSafeAreaInsets();
  return <View style={[s.bottomBar, { paddingBottom: Math.max(insets.bottom, 12) }]}>{children}</View>;
}

export default function App() {
  const { c, dark } = useTheme();
  return <SafeAreaProvider>
    <StatusBar barStyle={dark ? 'light-content' : 'dark-content'} />
    <SafeAreaView style={{ flex: 1, backgroundColor: c.background }} edges={['top', 'left', 'right']}>
      <Ledger />
    </SafeAreaView>
  </SafeAreaProvider>;
}

function Ledger() {
  const [phase, setPhase] = useState<Phase>('loading');
  const [lockReason, setLockReason] = useState('');
  const [mode, setMode] = useState<LedgerMode>('local');
  const [profile, setProfile] = useState<Profile | null>(null);
  const [showSync, setShowSync] = useState(false);
  const [busy, setBusy] = useState<Busy>(null);
  const [notice, setNotice] = useState<Notice>(null);
  const [items, setItems] = useState<Statement[]>([]);
  const [conflicts, setConflicts] = useState(0);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [defaults, setDefaults] = useState<{ source: string; currency: string; order: DateOrder }>({ source: '', currency: '', order: 'MDY' });
  const active = useRef(false);
  const share = useIncomingShare();
  // Without an account there is no server to talk to: the device ledger is the only ledger.
  const synced = mode === 'synced' && !!profile;
  const scope = phase !== 'ready' ? null : synced ? profile.scope : LOCAL_SCOPE;

  async function boot() {
    setPhase('loading');
    try {
      const [session, saved] = await Promise.all([storedProfile().catch(() => null), loadMode()]);
      setProfile(session);
      setMode(saved === 'synced' && session ? 'synced' : 'local');
      setPhase('ready');
    } catch (error) {
      if (error instanceof LockedLedgerError) {
        setLockReason(error.message);
        setPhase('locked');
      } else {
        setNotice({ tone: 'error', text: message(error) });
        setPhase('ready');
      }
    }
  }

  useEffect(() => { boot(); }, []);

  useEffect(() => {
    let current = true;
    setItems([]);
    setConflicts(0);
    if (scope) Promise.all([statements(scope), conflictCount(scope)]).then(([records, count]) => {
      if (current) { setItems(records); setConflicts(count); }
    }).catch((error: unknown) => { if (current) setNotice({ tone: 'error', text: message(error) }); });
    return () => { current = false; };
  }, [scope]);

  async function perform(key: Exclude<Busy, null>, work: () => Promise<void>) {
    if (active.current) return;
    active.current = true;
    setBusy(key);
    setNotice(null);
    try { await work(); }
    catch (error) { setNotice({ tone: 'error', text: message(error) }); }
    finally { active.current = false; setBusy(null); }
  }

  async function read(uri: string) {
    const document = await pdf.read(uri);
    const detected = detectDateOrder(document.text);
    setDraft({
      ...document, ...defaults, detected, rows: null, skipped: 0,
      order: detected === 'DMY' || detected === 'MDY' ? detected : defaults.order,
    });
  }

  useEffect(() => {
    const subscription = Linking.addEventListener('url', ({ url }) => {
      if (url.startsWith('peanutcount://expo-sharing')) share.refreshSharePayloads();
    });
    return () => subscription.remove();
  }, [share.refreshSharePayloads]);

  useEffect(() => {
    if (!scope || draft || active.current || share.isResolving) return;
    const resolved = share.resolvedSharedPayloads.find((item) =>
      item.contentType === 'file' && item.contentUri?.startsWith('file:'));
    const raw = share.sharedPayloads.find((item) => item.shareType === 'file' && item.value.startsWith('file:'));
    const uri = resolved?.contentUri ?? raw?.value;
    if (uri) {
      setShowSync(false);
      perform('import', async () => {
        try { await read(uri); }
        finally {
          share.clearSharedPayloads();
          share.refreshSharePayloads();
        }
      });
    } else if (share.sharedPayloads.length || share.error) {
      setNotice({ tone: 'warning', text: 'Share a PDF statement from Files or another app.' });
      share.clearSharedPayloads();
      share.refreshSharePayloads();
    }
  }, [scope, busy, draft, share.sharedPayloads, share.isResolving, share.error, share.resolvedSharedPayloads]);

  async function selectMode(next: LedgerMode) {
    await saveMode(next);
    setMode(next);
  }

  async function refresh(target: string) {
    const [records, count] = await Promise.all([statements(target), conflictCount(target)]);
    setItems(records);
    setConflicts(count);
  }

  async function synchronize(session: Profile) {
    try {
      await syncStatements(session);
    } finally {
      await refresh(session.scope);
    }
  }

  function preview() {
    if (!draft) return;
    const source = draft.source.trim();
    const currency = draft.currency.trim().toUpperCase();
    try {
      const { rows, skipped } = parseStatement(draft.text, currency, draft.order);
      if (!rows.length) throw new Error('No supported transaction rows found. Check the date format and currency.');
      if (rows.length > 500) throw new Error('Choose a statement with at most 500 transaction rows.');
      setNotice(null);
      setDraft({ ...draft, source, currency, rows, skipped });
    } catch (error) { setNotice({ tone: 'error', text: message(error) }); }
  }

  async function post(draft: ReviewDraft, scope: string) {
    const rows = draft.rows.filter((row) => !row.excluded).map(({ excluded: _, ...row }) => ({
      ...row, account: row.account.trim(),
    }));
    if (!rows.length || rows.some((row) => !validAccount(row.account) || row.account === draft.source)) {
      throw new Error('Select at least one transaction and enter a different valid account for every row.');
    }
    await addStatement(scope, { id: draft.id, source: draft.source, currency: draft.currency, rows }, synced);
    setDefaults({ source: draft.source, currency: draft.currency, order: draft.order });
    setDraft(null);
    await refresh(scope);
    const added = `Added ${plural(rows.length, 'transaction')}.`;
    if (!synced) {
      setNotice({ tone: 'success', text: added });
      return;
    }
    try {
      await synchronize(profile);
      setNotice({ tone: 'success', text: `${added} Synced.` });
    } catch (error) {
      setNotice({ tone: 'warning', text: `${added} Saved on this iPhone; upload will retry on the next sync. ${message(error)}` });
    }
  }

  const known = useMemo(() => ({
    sources: [...new Set(items.map((statement) => statement.source))].slice(0, 6),
    currencies: [...new Set(items.map((statement) => statement.currency))].slice(0, 4),
  }), [items]);

  if (phase === 'loading') return <View style={{ flex: 1, justifyContent: 'center' }}><ActivityIndicator /></View>;

  if (phase === 'locked') return <LockedScreen reason={lockReason} onErase={async () => {
    await eraseLocalLedger();
    await boot();
  }} />;

  const banner = notice ? <Banner tone={notice.tone} text={notice.text} onDismiss={() => setNotice(null)} /> : null;

  // Past the loading and locked phases, the ledger scope is always known.
  if (!scope) return null;

  if (draft?.rows) {
    const review: ReviewDraft = { ...draft, rows: draft.rows };
    return <ReviewScreen
      draft={review} setDraft={setDraft} banner={banner} busy={busy}
      onBack={() => { setNotice(null); setDraft({ ...review, rows: null }); }}
      onSubmit={() => {
        const confirm = () => perform('post', () => post(review, scope));
        if (review.skipped) Alert.alert('Import the parsed transactions?', `${plural(review.skipped, 'dated line')} could not be read and will be left out.`, [
          { text: 'Cancel', style: 'cancel' }, { text: 'Import', onPress: confirm },
        ]);
        else confirm();
      }}
    />;
  }

  if (draft) return <SetupScreen
    draft={draft} setDraft={setDraft} known={known} banner={banner}
    onCancel={() => { setNotice(null); setDraft(null); }}
    onContinue={preview}
  />;

  return <>
    <HomeScreen
      items={items} synced={synced} profile={profile} conflicts={conflicts} busy={busy} banner={banner}
      onImport={() => perform('import', async () => {
        const result = await DocumentPicker.getDocumentAsync({ type: 'application/pdf', copyToCacheDirectory: true });
        if (!result.canceled) await read(result.assets[0].uri);
      })}
      onOpenSync={() => { setNotice(null); setShowSync(true); }}
      onSync={() => perform('sync', async () => {
        if (!profile) return;
        await synchronize(profile);
        setNotice({ tone: 'success', text: 'Up to date.' });
      })}
      onResolve={() => Alert.alert('Use the server copies?', 'This iPhone’s versions of the conflicting imports will be archived on this device.', [
        { text: 'Cancel', style: 'cancel' },
        { text: 'Use server copies', onPress: () => perform('sync', async () => {
          if (!profile) return;
          await resolveConflicts(scope);
          await synchronize(profile);
        }) },
      ])}
    />
    <SyncSheet
      visible={showSync} onClose={() => setShowSync(false)}
      profile={profile} synced={synced} busy={busy} banner={banner}
      onSync={() => perform('sync', async () => {
        if (!profile) return;
        await synchronize(profile);
        setNotice({ tone: 'success', text: 'Up to date.' });
      })}
      onSelectLedger={(next) => perform('mode', () => selectMode(next))}
      onAuthenticate={(kind, address, username, password) => perform('auth', async () => {
        const session = await account(kind, address, username, password);
        setProfile(session);
        await selectMode('synced');
        setShowSync(false);
        await synchronize(session);
      })}
      onSignOut={() => Alert.alert('Sign out?', 'The synced ledger stays on the server. This iPhone switches back to its encrypted on-device ledger.', [
        { text: 'Cancel', style: 'cancel' },
        { text: 'Sign out', style: 'destructive', onPress: () => perform('signout', async () => {
          await clearProfile();
          setProfile(null);
          await selectMode('local');
        }) },
      ])}
    />
  </>;
}

function HomeScreen({ items, synced, profile, conflicts, busy, banner, onImport, onOpenSync, onSync, onResolve }: {
  items: Statement[];
  synced: boolean;
  profile: Profile | null;
  conflicts: number;
  busy: Busy;
  banner: ReactElement | null;
  onImport: () => void;
  onOpenSync: () => void;
  onSync: () => void;
  onResolve: () => void;
}) {
  const { c, s } = useTheme();
  const rows = useMemo(() => items
    .flatMap((statement) => statement.rows.map((row, index) => ({
      ...row, source: statement.source, currency: statement.currency, key: `${statement.id}:${index}`,
    })))
    .sort((a, b) => b.date.localeCompare(a.date)), [items]);
  const [chosenMonth, setMonth] = useState<string | null>(null);
  const [currency, setCurrency] = useState<string | null>(null);
  const first = rows.at(-1)?.date.slice(0, 7);
  const last = rows[0]?.date.slice(0, 7);
  // Follow the newest month by default, and again after an import adds a newer one.
  const month = chosenMonth && first && last && chosenMonth >= first && chosenMonth <= last ? chosenMonth : last;
  useEffect(() => { setMonth(null); }, [last]);
  // The list shows the month picked in the spending card, so the two always agree.
  const sections = useMemo(() => {
    const data = rows.filter((row) => row.date.startsWith(month ?? ''));
    return month ? [{ title: monthTitle(`${month}-01`), data }] : [];
  }, [rows, month]);
  const accounts = new Set(items.map((statement) => statement.source)).size;

  const header = <View style={{ gap: 16, paddingTop: 8 }}>
    <View style={[s.row, { alignItems: 'flex-start' }]}>
      <View style={{ flex: 1, gap: 6 }}>
        <Text style={s.largeTitle} accessibilityRole="header">Peanut Count</Text>
        <View style={[s.row, { gap: 8, flexWrap: 'wrap' }]}>
          <Pill label="Encrypted" />
          <Pill tone={synced ? 'accent' : 'muted'} label={synced && profile ? `Synced · ${profile.username}` : 'On this iPhone only'} />
        </View>
      </View>
      <View style={{ marginTop: 4 }}>
        <Button compact variant="plain" label={profile ? 'Account' : 'Sync'} onPress={onOpenSync} />
      </View>
    </View>

    <SpendingCard statements={items} month={month} first={first} last={last} onMonthChange={setMonth}
      currency={currency} onCurrencyChange={setCurrency} />

    <View style={{ gap: 8 }}>
      <Button label="Import statement PDF" loading={busy === 'import'} disabled={!!busy} onPress={onImport} />
      <Text style={[s.footnote, { textAlign: 'center' }]}>Or share a PDF to Peanut Count from Files, Mail or Safari.</Text>
    </View>

    {conflicts > 0 && <Banner tone="warning" text={`${plural(conflicts, 'import')} on this iPhone differ from the server copy.`}>
      <Button compact variant="secondary" label="Use server copies" disabled={!!busy} onPress={onResolve} />
    </Banner>}
    {banner}
  </View>;

  return <SectionList
    sections={sections}
    keyExtractor={(item) => item.key}
    contentContainerStyle={{ paddingHorizontal: 20, paddingBottom: 48 }}
    stickySectionHeadersEnabled={false}
    refreshControl={synced ? <RefreshControl refreshing={busy === 'sync'} onRefresh={onSync} tintColor={c.muted} /> : undefined}
    ListHeaderComponent={header}
    ListEmptyComponent={<View style={{ marginTop: 16 }}><EmptyState synced={synced} /></View>}
    renderSectionHeader={({ section }) => <View style={{ marginTop: 24, marginBottom: 8, gap: 2 }}>
      <Text style={[s.sectionLabel, { marginTop: 0, marginBottom: 0 }]}>{section.title} transactions</Text>
      {section.data.length === 0 && <Text style={[s.secondary, { marginLeft: 4, marginTop: 6 }]}>No transactions this month.</Text>}
    </View>}
    renderItem={({ item, index, section }) => <TransactionRow item={item} showSource={accounts > 1}
      first={index === 0} last={index === section.data.length - 1} />}
  />;
}

function Stat({ value, label }: { value: string; label: string }) {
  const { s } = useTheme();
  return <View style={s.stat}>
    <Text style={s.statValue} numberOfLines={1} adjustsFontSizeToFit>{value}</Text>
    <Text style={s.statLabel}>{label}</Text>
  </View>;
}

type TransactionItem = StatementRow & { source: string; currency: string; key: string };

function TransactionRow({ item, showSource, first, last }: { item: TransactionItem; showSource: boolean; first: boolean; last: boolean }) {
  const { c, s } = useTheme();
  const inflow = !item.amount.startsWith('-');
  return <View style={{
    backgroundColor: c.surface, paddingHorizontal: 16, paddingVertical: 12,
    borderTopLeftRadius: first ? 16 : 0, borderTopRightRadius: first ? 16 : 0,
    borderBottomLeftRadius: last ? 16 : 0, borderBottomRightRadius: last ? 16 : 0,
    borderBottomWidth: last ? 0 : StyleSheet.hairlineWidth, borderBottomColor: c.border,
  }} accessible accessibilityLabel={`${item.description}, ${shortDate(item.date)}, ${formatAmount(item.amount)} ${item.currency}, ${item.account}`}>
    <View style={s.row}>
      <View style={{ flex: 1, gap: 2 }}>
        <Text style={s.headline} numberOfLines={1}>{item.description}</Text>
        <Text style={s.footnote} numberOfLines={1}>{shortDate(item.date)} · {item.account}</Text>
        {showSource && <Text style={s.footnote} numberOfLines={1}>{item.source}</Text>}
      </View>
      <View style={{ alignItems: 'flex-end' }}>
        <Text style={[s.amount, inflow && { color: c.positive }]}>{formatAmount(item.amount)}</Text>
        <Text style={s.footnote}>{item.currency}</Text>
      </View>
    </View>
  </View>;
}

function EmptyState({ synced }: { synced: boolean }) {
  const { s } = useTheme();
  const steps = [
    ['Import a statement', 'Choose a PDF above, or share one from another app. Text is read on this iPhone.'],
    ['Confirm the account', 'Tell Peanut Count which account the statement belongs to.'],
    ['Review and add', 'Check each transaction’s category before it goes into your ledger.'],
  ];
  return <View style={[s.card, { gap: 16 }]}>
    <Text style={s.title}>Your ledger is empty</Text>
    {steps.map(([title, body], index) => <View key={title} style={[s.row, { alignItems: 'flex-start' }]}>
      <Text style={[s.statValue, { width: 24, fontSize: 17 }]}>{index + 1}</Text>
      <View style={{ flex: 1, gap: 2 }}>
        <Text style={s.headline}>{title}</Text>
        <Text style={s.secondary}>{body}</Text>
      </View>
    </View>)}
    {!synced && <Text style={s.footnote}>Everything stays encrypted on this iPhone. Sync is optional and off until you sign in.</Text>}
  </View>;
}

function SetupScreen({ draft, setDraft, known, banner, onCancel, onContinue }: {
  draft: Draft;
  setDraft: SetDraft;
  known: { sources: string[]; currencies: string[] };
  banner: ReactElement | null;
  onCancel: () => void;
  onContinue: () => void;
}) {
  const { s } = useTheme();
  const update = (patch: Partial<Draft>) => setDraft((previous) => previous && { ...previous, ...patch });
  const source = draft.source.trim();
  const currency = draft.currency.trim().toUpperCase();
  const sourceInvalid = source.length > 0 && !isStatementAccount(source);
  const ready = isStatementAccount(source) && currencyPattern.test(currency);
  // Only ask about the date format when the statement's dates leave it open. When it
  // is ambiguous, ask in the bottom bar, which rides above the keyboard and never scrolls away.
  const dateFormat = draft.detected === 'ISO'
    ? <Text style={s.footnote}>Dates on this statement use YYYY-MM-DD.</Text>
    : <Segmented label="Date format on the statement" value={draft.order} onChange={(order) => update({ order })}
      options={[{ value: 'MDY', label: 'MM/DD/YYYY' }, { value: 'DMY', label: 'DD/MM/YYYY' }]} />;
  const ambiguous = !draft.detected;

  return <KeyboardAvoidingView behavior="padding" style={{ flex: 1 }}>
    <NavBar left={<Button compact variant="plain" label="Cancel" onPress={onCancel} />} title="Import" />
    <ScrollView contentContainerStyle={s.content} keyboardShouldPersistTaps="handled" keyboardDismissMode="interactive">
      <View style={{ gap: 4, marginTop: 8 }}>
        <Text style={s.eyebrow}>Step 1 of 2</Text>
        <Text style={s.title}>Statement details</Text>
        <Text style={s.secondary}>The PDF was read on this iPhone. It is never uploaded.</Text>
      </View>

      <View style={s.card}>
        <Field label="Statement account" placeholder="Assets:Checking" value={draft.source}
          onChangeText={(value) => update({ source: value })} invalid={sourceInvalid} returnKeyType="next"
          hint={sourceInvalid ? 'Use an Assets: or Liabilities: account, e.g. Liabilities:CreditCard.' : 'The account this statement belongs to.'} />
        <Chips values={known.sources.filter((value) => value !== source)} onSelect={(value) => update({ source: value })} />
        <View style={s.divider} />
        <Field label="Currency" placeholder="USD" value={draft.currency} maxLength={3}
          onChangeText={(value) => update({ currency: value.toUpperCase() })} autoCapitalize="characters" returnKeyType="done" />
        <Chips values={known.currencies.filter((value) => value !== currency)} onSelect={(value) => update({ currency: value })} />
        {!ambiguous && <View style={s.divider} />}
        {!ambiguous && dateFormat}
        {!ambiguous && draft.detected !== 'ISO' && <Text style={s.footnote}>Detected from the dates on the statement.</Text>}
      </View>
      {banner}
    </ScrollView>
    <BottomBar>
      {ambiguous && <View style={{ gap: 6, marginBottom: 4 }}>
        {dateFormat}
        <Text style={s.footnote}>Look for a date with a day above 12 to tell which format the statement uses.</Text>
      </View>}
      <Button label="Find transactions" disabled={!ready} onPress={onContinue} />
    </BottomBar>
  </KeyboardAvoidingView>;
}

function ReviewScreen({ draft, setDraft, banner, busy, onBack, onSubmit }: {
  draft: ReviewDraft;
  setDraft: SetDraft;
  banner: ReactElement | null;
  busy: Busy;
  onBack: () => void;
  onSubmit: () => void;
}) {
  const { c, s } = useTheme();
  const included = draft.rows.filter((row) => !row.excluded);
  const totals = included.reduce(([inflow, outflow], row) => {
    const cents = toCents(row.amount);
    return cents > 0n ? [inflow + cents, outflow] : [inflow, outflow + cents];
  }, [0n, 0n]);
  const rowInvalid = (row: DraftRow): boolean => !validAccount(row.account.trim()) || row.account.trim() === draft.source;
  const invalid = included.filter(rowInvalid).length;
  const edit = (index: number, patch: Partial<DraftRow>) => setDraft((previous) => previous?.rows ? {
    ...previous,
    rows: previous.rows.map((row, position) => position === index ? { ...row, ...patch } : row),
  } : previous);

  return <KeyboardAvoidingView behavior="padding" style={{ flex: 1 }}>
    <NavBar left={<Button compact variant="plain" label="‹ Back" disabled={!!busy} onPress={onBack} />} title="Review" />
    <FlatList
      data={draft.rows}
      keyboardShouldPersistTaps="handled"
      keyboardDismissMode="interactive"
      contentContainerStyle={[s.content, { gap: 10 }]}
      keyExtractor={(_, index) => String(index)}
      ListHeaderComponent={<View style={{ gap: 16, marginTop: 8, marginBottom: 6 }}>
        <View style={{ gap: 4 }}>
          <Text style={s.eyebrow}>Step 2 of 2</Text>
          <Text style={s.title}>{plural(draft.rows.length, 'transaction')} found</Text>
          <Text style={s.secondary}>{draft.source} · {draft.currency}</Text>
        </View>
        <View style={[s.card, s.row, { gap: 0 }]}>
          <Stat value={formatCents(totals[0])} label="Money in" />
          <Stat value={formatCents(totals[1])} label="Money out" />
        </View>
        {draft.skipped > 0 && <Banner tone="warning"
          text={`${plural(draft.skipped, 'dated line')} could not be read. Compare these transactions with the PDF before adding them.`} />}
      </View>}
      renderItem={({ item, index }) => {
        const position = draft.rows.slice(0, index).filter((row) => !row.excluded).length;
        const bad = !item.excluded && rowInvalid(item);
        return <View style={[s.card, item.excluded && { opacity: 0.55 }]}>
          <View style={[s.row, { alignItems: 'flex-start' }]}>
            <View style={{ flex: 1, gap: 2 }}>
              <Text style={s.headline} numberOfLines={2}>{item.description}</Text>
              <Text style={s.footnote}>{shortDate(item.date)}, {item.date.slice(0, 4)}</Text>
            </View>
            <Text style={[s.amount, !item.amount.startsWith('-') && { color: c.positive }]}>{formatAmount(item.amount)}</Text>
          </View>
          {!item.excluded && <Field label="Category" placeholder="Expenses:Groceries" value={item.account} invalid={bad}
            accessibilityLabel={`Category for ${item.description}`}
            hint={bad ? 'Enter a valid account that differs from the statement account.'
              : position === 0 ? 'The account this money came from or went to, e.g. Expenses:Dining.' : undefined}
            onChangeText={(value) => edit(index, { account: value })} />}
          <View style={s.row}>
            <Text style={[s.secondary, { flex: 1 }]}>Include in ledger</Text>
            <Switch value={!item.excluded} onValueChange={(value) => edit(index, { excluded: !value })}
              trackColor={{ true: c.accent }} accessibilityLabel={`Include ${item.description}`} />
          </View>
        </View>;
      }}
      ListFooterComponent={banner}
    />
    <BottomBar>
      {invalid > 0 && <Text style={[s.footnote, { textAlign: 'center', color: c.danger }]}>{plural(invalid, 'row')} need a valid account.</Text>}
      <Button label={included.length ? `Add ${plural(included.length, 'transaction')}` : 'Select transactions to add'}
        loading={busy === 'post'} disabled={!included.length || invalid > 0 || !!busy} onPress={onSubmit} />
    </BottomBar>
  </KeyboardAvoidingView>;
}

function SyncSheet({ visible, onClose, profile, synced, busy, banner, onSync, onSelectLedger, onAuthenticate, onSignOut }: {
  visible: boolean;
  onClose: () => void;
  profile: Profile | null;
  synced: boolean;
  busy: Busy;
  banner: ReactElement | null;
  onSync: () => void;
  onSelectLedger: (mode: LedgerMode) => void;
  onAuthenticate: (kind: AuthMode, address: string, username: string, password: string) => void;
  onSignOut: () => void;
}) {
  const { c, s } = useTheme();
  return <Modal visible={visible} animationType="slide" presentationStyle="pageSheet" onRequestClose={onClose}>
    <View style={[s.screen, { paddingTop: 8 }]}>
      <NavBar title={profile ? 'Account' : 'Sync'} right={<Button compact variant="plain" label="Done" onPress={onClose} />} />
      <KeyboardAvoidingView behavior="padding" style={{ flex: 1 }}>
        <ScrollView contentContainerStyle={[s.content, { paddingTop: 12 }]} keyboardShouldPersistTaps="handled">
          <Text style={s.sectionLabel}>Storage</Text>
          <View style={s.card}>
            <Text style={s.headline}>Encrypted on this iPhone</Text>
            <Text style={s.secondary}>
              Your ledger is kept in an AES-256 encrypted database. Its key is generated on this iPhone and stored in the iOS Keychain.
            </Text>
          </View>

          {profile ? <>
            <Text style={s.sectionLabel}>Signed in</Text>
            <View style={s.card}>
              <View style={{ gap: 2 }}>
                <Text style={s.headline}>{profile.username}</Text>
                <Text style={s.secondary}>{profile.server.replace(/^https:\/\//, '')}</Text>
              </View>
              <Segmented label="Show ledger" value={synced ? 'synced' : 'local'} onChange={onSelectLedger}
                options={[{ value: 'local', label: 'This iPhone' }, { value: 'synced', label: 'Synced' }]} />
              <Text style={s.footnote}>
                The on-device ledger and the synced ledger are kept apart. Imports made on this iPhone before signing in are never uploaded.
              </Text>
            </View>
            {synced && <Button variant="secondary" label="Sync now" loading={busy === 'sync'} disabled={!!busy} onPress={onSync} />}
            {banner}
            <Button variant="destructive" label="Sign out" loading={busy === 'signout'} disabled={!!busy} onPress={onSignOut} />
          </> : <>
            <Text style={s.sectionLabel}>Sync</Text>
            <View style={s.card}>
              <Pill tone="muted" label="Off" />
              <Text style={s.secondary}>
                Without an account, Peanut Count never connects to a server. Your ledger works fully offline and stays on this iPhone.
              </Text>
              <Text style={s.secondary}>
                To use the same ledger on several devices, sign in to a sync server you host.
              </Text>
            </View>
            <AuthForm busy={busy} onSubmit={onAuthenticate} />
            {banner}
          </>}
          <Text style={[s.footnote, { color: c.faint, textAlign: 'center' }]}>PDF statements are read on-device and are never uploaded.</Text>
        </ScrollView>
      </KeyboardAvoidingView>
    </View>
  </Modal>;
}

function AuthForm({ busy, onSubmit }: {
  busy: Busy;
  onSubmit: (kind: AuthMode, address: string, username: string, password: string) => void;
}) {
  const { s } = useTheme();
  const [kind, setKind] = useState<AuthMode>('login');
  const [address, setAddress] = useState('');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  return <View style={s.card}>
    <Segmented value={kind} onChange={setKind}
      options={[{ value: 'login', label: 'Sign in' }, { value: 'signup', label: 'Create account' }]} />
    <Field label="Sync server" placeholder="https://ledger.example.com" value={address} onChangeText={setAddress}
      keyboardType="url" textContentType="URL" />
    <Field label="Username" value={username} onChangeText={setUsername} textContentType="username" />
    <Field label="Password" value={password} onChangeText={setPassword} secureTextEntry
      textContentType={kind === 'signup' ? 'newPassword' : 'password'} />
    <Button label={kind === 'signup' ? 'Create account and sync' : 'Sign in and sync'} loading={busy === 'auth'}
      disabled={!address || !username || !password || !!busy} onPress={() => onSubmit(kind, address, username, password)} />
  </View>;
}

function LockedScreen({ reason, onErase }: { reason: string; onErase: () => Promise<void> }) {
  const { s } = useTheme();
  const [working, setWorking] = useState(false);
  return <ScrollView contentContainerStyle={[s.content, { paddingTop: 48 }]}>
    <Text style={s.largeTitle}>Ledger locked</Text>
    <Text style={s.body}>{reason}</Text>
    <Text style={s.secondary}>
      The encryption key for the on-device ledger is missing from the Keychain, so its data cannot be read.
      This can happen after restoring from an unencrypted backup. Data in a synced account is safe on its server.
    </Text>
    <Button variant="destructive" label="Erase on-device ledger" loading={working} onPress={() => Alert.alert(
      'Erase the on-device ledger?', 'Imports stored only on this iPhone will be permanently deleted.', [
        { text: 'Cancel', style: 'cancel' },
        { text: 'Erase', style: 'destructive', onPress: async () => {
          setWorking(true);
          try { await onErase(); }
          catch (error) { Alert.alert('Could not erase the ledger', message(error)); }
          finally { setWorking(false); }
        } },
      ])} />
  </ScrollView>;
}
