import React, { useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Alert, FlatList, Linking, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';
import { SafeAreaProvider, SafeAreaView } from 'react-native-safe-area-context';
import { requireNativeModule } from 'expo';
import * as DocumentPicker from 'expo-document-picker';
import { useIncomingShare } from 'expo-sharing';
import { account, clearProfile, storedProfile, syncStatements } from './api';
import { addStatement, conflictCount, loadMode, resolveConflicts, saveMode, statements } from './storage';

const { validAccount, currencyPattern, parseStatement } = require('./parser.cjs');
const pdf = requireNativeModule('StatementPdf');

export default function App() {
  return <SafeAreaProvider><SafeAreaView style={styles.safe}><Ledger /></SafeAreaView></SafeAreaProvider>;
}

function Ledger() {
  const [mode, setMode] = useState('loading');
  const [profile, setProfile] = useState(null);
  const [auth, setAuth] = useState(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [items, setItems] = useState([]);
  const [conflicts, setConflicts] = useState(0);
  const [draft, setDraft] = useState(null);
  const [defaults, setDefaults] = useState({ source: '', currency: '', order: 'MDY' });
  const active = useRef(false);
  const share = useIncomingShare();
  const scope = mode === 'synced' ? profile?.scope : mode === 'local' ? 'device' : null;

  useEffect(() => {
    Promise.all([storedProfile().catch(() => null), loadMode()]).then(([session, saved]) => {
      setProfile(session);
      setMode(saved === 'local' ? 'local' : saved === 'synced' && session ? 'synced' : 'welcome');
    }).catch((error) => { setMessage(error.message); setMode('welcome'); });
  }, []);

  useEffect(() => {
    let current = true;
    setItems([]);
    setConflicts(0);
    if (scope) Promise.all([statements(scope), conflictCount(scope)]).then(([records, count]) => {
      if (current) { setItems(records); setConflicts(count); }
    }).catch((error) => { if (current) setMessage(error.message); });
    return () => { current = false; };
  }, [scope]);

  async function perform(work) {
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setMessage('');
    try { await work(); }
    catch (error) { setMessage(error.message); }
    finally { active.current = false; setBusy(false); }
  }

  async function read(uri) {
    const document = await pdf.read(uri);
    setDraft({
      ...document,
      ...defaults,
      rows: null,
      skipped: 0,
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
      perform(async () => {
        try { await read(uri); }
        finally {
          share.clearSharedPayloads();
          share.refreshSharePayloads();
        }
      });
    } else if (share.sharedPayloads.length || share.error) {
      setMessage('Share a local PDF statement from Files or another app.');
      share.clearSharedPayloads();
      share.refreshSharePayloads();
    }
  }, [scope, busy, draft, share.sharedPayloads, share.isResolving, share.error, share.resolvedSharedPayloads]);

  async function selectMode(next) {
    await saveMode(next);
    setMode(next);
    setAuth(null);
  }

  async function refresh() {
    const [records, count] = await Promise.all([statements(scope), conflictCount(scope)]);
    setItems(records);
    setConflicts(count);
  }

  async function synchronize() {
    if (mode !== 'synced') return;
    try {
      await syncStatements(profile);
    } finally {
      await refresh();
    }
  }

  function preview() {
    const source = draft.source.trim();
    if (!validAccount(source) || !/^(Assets|Liabilities):/.test(source)) {
      setMessage('Enter a statement account such as Assets:Checking or Liabilities:CreditCard.');
      return;
    }
    try {
      const { rows, skipped } = parseStatement(draft.text, draft.currency.trim().toUpperCase(), draft.order);
      if (!rows.length) throw new Error('No supported transaction rows found. Check the date order and statement currency.');
      if (rows.length > 500) throw new Error('Choose a statement with at most 500 transaction rows.');
      setMessage('');
      setDraft({ ...draft, source, currency: draft.currency.trim().toUpperCase(), rows, skipped });
    } catch (error) { setMessage(error.message); }
  }

  async function post() {
    const rows = draft.rows.filter((row) => !row.excluded).map(({ excluded, ...row }) => ({
      ...row, account: row.account.trim(),
    }));
    if (!rows.length || rows.some((row) => !validAccount(row.account) || row.account === draft.source)) {
      throw new Error('Select at least one transaction and enter a different valid account for every row.');
    }
    await addStatement(scope, { id: draft.id, source: draft.source, currency: draft.currency, rows }, mode === 'synced');
    setDefaults({ source: draft.source, currency: draft.currency, order: draft.order });
    setDraft(null);
    await refresh();
    await synchronize();
    setMessage(`Added ${rows.length} ${rows.length === 1 ? 'transaction' : 'transactions'}.`);
  }

  const updateDraft = (patch) => setDraft((previous) => ({ ...previous, ...patch }));
  const controls = { busy };
  const status = message ? <Text accessibilityLiveRegion="polite" style={styles.message}>{message}</Text> : null;

  if (mode === 'loading') return <ActivityIndicator style={styles.container} />;

  if (!scope) return (
    <ScrollView contentContainerStyle={styles.container} keyboardShouldPersistTaps="handled">
      <Text style={styles.title}>Peanut Count</Text>
      <Text style={styles.body}>Import PDF statements into your ledger.</Text>
      {auth ? <AuthForm busy={busy} mode={auth} onSubmit={(address, username, password) => perform(async () => {
        const session = await account(auth, address, username, password);
        setProfile(session);
        await selectMode('synced');
        await syncStatements(session).finally(async () => {
          setItems(await statements(session.scope));
          setConflicts(await conflictCount(session.scope));
        });
      })} /> : (
        <>
          {profile && <Button label={`Open ${profile.username}'s ledger`} {...controls} onPress={() => perform(() => selectMode('synced'))} />}
          <Button label="Sign up" {...controls} onPress={() => setAuth('signup')} />
          <Button label="Sign in" secondary {...controls} onPress={() => setAuth('login')} />
        </>
      )}
      {auth && <Button label="Back" secondary {...controls} onPress={() => setAuth(null)} />}
      <Button label="Continue without sync" secondary {...controls} onPress={() => perform(() => selectMode('local'))} />
      {status}
    </ScrollView>
  );

  if (draft?.rows) return (
    <FlatList
      data={draft.rows}
      keyboardShouldPersistTaps="handled"
      contentContainerStyle={styles.container}
      keyExtractor={(_, index) => String(index)}
      ListHeaderComponent={<>
        <Text style={styles.title}>Review transactions</Text>
        <Text style={styles.body}>{draft.source} · {draft.currency}</Text>
        {draft.skipped > 0 && <Text style={styles.message}>{draft.skipped} dated lines could not be parsed. Compare these transactions with the PDF before adding them.</Text>}
      </>}
      renderItem={({ item, index }) => {
        const edit = (patch) => setDraft((previous) => ({
          ...previous,
          rows: previous.rows.map((row, position) => position === index ? { ...row, ...patch } : row),
        }));
        return <View style={styles.card}>
          <Text style={styles.body}>{item.date}  {item.amount} {draft.currency}</Text>
          <Text style={styles.label}>{item.description}</Text>
          {!item.excluded && <Field label="Counter-account" value={item.account} onChangeText={(value) => edit({ account: value })} />}
          <Button label={item.excluded ? 'Include transaction' : 'Exclude transaction'} secondary {...controls} onPress={() => edit({ excluded: !item.excluded })} />
        </View>;
      }}
      ListFooterComponent={<View style={styles.stack}>
        <Button label="Add transactions" {...controls} onPress={() => {
          const confirm = () => perform(post);
          if (draft.skipped) Alert.alert('Import parsed transactions?', 'Unparsed lines will be left out.', [
            { text: 'Cancel', style: 'cancel' }, { text: 'Import', onPress: confirm },
          ]);
          else confirm();
        }} />
        <Button label="Back to statement settings" secondary {...controls} onPress={() => updateDraft({ rows: null })} />
        {status}
      </View>}
    />
  );

  if (draft) return (
    <ScrollView contentContainerStyle={styles.container} keyboardShouldPersistTaps="handled">
      <Text style={styles.title}>Import statement</Text>
      <Field label="Statement account" placeholder="Assets:Checking" value={draft.source} onChangeText={(source) => updateDraft({ source })} />
      <Field label="Currency" placeholder="USD" value={draft.currency} onChangeText={(currency) => updateDraft({ currency })} autoCapitalize="characters" />
      <Button label={draft.order === 'MDY' ? 'Date order: month/day/year' : 'Date order: day/month/year'} secondary {...controls}
        onPress={() => updateDraft({ order: draft.order === 'MDY' ? 'DMY' : 'MDY' })} />
      <Button label="Parse statement" {...controls} disabled={!currencyPattern.test(draft.currency.trim().toUpperCase()) || !draft.source.trim()} onPress={preview} />
      <Button label="Cancel" secondary {...controls} onPress={() => setDraft(null)} />
      {status}
    </ScrollView>
  );

  return (
    <FlatList
      data={items.flatMap((statement) => statement.rows.map((row, index) => ({ ...row, source: statement.source, currency: statement.currency, key: `${statement.id}:${index}` })))
        .sort((a, b) => b.date.localeCompare(a.date))}
      keyExtractor={(item) => item.key}
      contentContainerStyle={styles.container}
      ListHeaderComponent={<View style={styles.stack}>
        <Text style={styles.title}>Peanut Count</Text>
        <Text style={styles.label}>{mode === 'local' ? 'On-device ledger' : `Synced ledger · ${profile.username}`}</Text>
        <Button label="Choose a PDF" {...controls} onPress={() => perform(async () => {
          const result = await DocumentPicker.getDocumentAsync({ type: 'application/pdf', copyToCacheDirectory: true });
          if (!result.canceled) await read(result.assets[0].uri);
        })} />
        {mode === 'synced' && <Button label="Sync now" secondary {...controls} onPress={() => perform(synchronize)} />}
        {conflicts > 0 && <Button label="Use server copies for conflicts" secondary {...controls} onPress={() => {
          Alert.alert('Use server copies?', 'The current device copies will be archived locally.', [
            { text: 'Cancel', style: 'cancel' },
            { text: 'Use server copies', onPress: () => perform(async () => {
              await resolveConflicts(scope);
              await synchronize();
            }) },
          ]);
        }} />}
        <Button label="Switch ledger" secondary {...controls} onPress={() => perform(() => selectMode('welcome'))} />
        {profile && <Button label="Sign out" secondary {...controls} onPress={() => perform(async () => {
          await clearProfile();
          setProfile(null);
          await selectMode('welcome');
        })} />}
        {busy && <ActivityIndicator />}
        {status}
      </View>}
      ListEmptyComponent={<Text style={styles.body}>Share a PDF from Files or choose one here to start.</Text>}
      renderItem={({ item }) => <View style={styles.card}>
        <Text style={styles.label}>{item.description}</Text>
        <Text style={styles.body}>{item.date} · {item.amount} {item.currency}</Text>
        <Text style={styles.body}>{item.source} / {item.account}</Text>
      </View>}
    />
  );
}

function AuthForm({ busy, mode, onSubmit }) {
  const [address, setAddress] = useState('');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  return <>
    <Field label="Sync server" placeholder="https://ledger.example.com" value={address} onChangeText={setAddress} keyboardType="url" />
    <Field label="Username" value={username} onChangeText={setUsername} textContentType="username" />
    <Field label="Password" value={password} onChangeText={setPassword} secureTextEntry textContentType={mode === 'signup' ? 'newPassword' : 'password'} />
    <Button label={mode === 'signup' ? 'Create account' : 'Sign in'} busy={busy}
      disabled={!address || !username || !password} onPress={() => onSubmit(address, username, password)} />
  </>;
}

function Field({ label, autoCapitalize = 'none', ...properties }) {
  return <View style={styles.field}>
    <Text style={styles.label}>{label}</Text>
    <TextInput accessibilityLabel={label} autoCapitalize={autoCapitalize} autoCorrect={false}
      placeholderTextColor="#657067" style={styles.input} {...properties} />
  </View>;
}

function Button({ label, onPress, busy, disabled, secondary }) {
  return <Pressable accessibilityRole="button" disabled={busy || disabled} onPress={onPress}
    style={[styles.button, secondary && styles.secondary, (busy || disabled) && styles.disabled]}>
    <Text style={[styles.buttonLabel, secondary && styles.secondaryLabel]}>{label}</Text>
  </Pressable>;
}

const styles = StyleSheet.create({
  safe: { flex: 1, backgroundColor: '#f4f7f0' },
  container: { padding: 24, gap: 16, paddingBottom: 56 },
  stack: { gap: 12 },
  field: { gap: 5 },
  title: { fontSize: 34, fontWeight: '800', color: '#183624', marginVertical: 8 },
  label: { fontSize: 16, fontWeight: '600', color: '#253b2c' },
  body: { fontSize: 16, color: '#34483a' },
  message: { fontSize: 15, color: '#6f3908', paddingVertical: 6 },
  input: { minHeight: 48, backgroundColor: '#fff', borderColor: '#b6c7b8', borderWidth: 1, borderRadius: 10, paddingHorizontal: 12, color: '#203327' },
  button: { minHeight: 46, borderRadius: 10, alignItems: 'center', justifyContent: 'center', backgroundColor: '#245432', padding: 10 },
  buttonLabel: { color: '#fff', fontSize: 16, fontWeight: '600' },
  secondary: { backgroundColor: '#e1ece0' },
  secondaryLabel: { color: '#254631' },
  disabled: { opacity: 0.55 },
  card: { gap: 7, borderRadius: 12, backgroundColor: '#fff', padding: 14 },
});
