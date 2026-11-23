import { fetch } from 'expo/fetch';
import * as Crypto from 'expo-crypto';
import * as SecureStore from 'expo-secure-store';
import { applyPage, conflictCount, getCursor, markSent, pendingStatements, type Statement } from './storage';

export interface Profile {
  server: string;
  scope: string;
  username: string;
  token: string;
}

export type AuthMode = 'login' | 'signup';

interface LoginResponse {
  username: string;
  token: string;
  ledger_id: string;
}

interface StatementPage {
  statements: Statement[];
  after: number;
}

const SESSION_KEY = 'peanut.account';

function serverUrl(input: string): string {
  let url: URL;
  try {
    url = new URL(input.trim());
  } catch {
    throw new Error('Enter an HTTPS sync server URL.');
  }
  if (url.protocol !== 'https:' || !url.hostname || url.username || url.password || url.search || url.hash) {
    throw new Error('Enter an HTTPS sync server URL without credentials or query parameters.');
  }
  return url.toString().replace(/\/+$/, '');
}

class RequestError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(server: string, path: string, { token, body }: { token?: string; body?: unknown } = {}): Promise<T> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 20000);
  try {
    const response = await fetch(`${server}/v1/${path}`, {
      method: body ? 'POST' : 'GET',
      headers: {
        Accept: 'application/json',
        ...(body ? { 'Content-Type': 'application/json' } : {}),
        ...(token ? { Authorization: `Bearer ${token}` } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
      redirect: 'error',
      signal: controller.signal,
    });
    if (!response.ok) {
      const payload: { error?: string } = await response.json().catch(() => ({}));
      throw new RequestError(response.status, payload.error || `Server returned HTTP ${response.status}.`);
    }
    return (response.status === 204 ? null : await response.json()) as T;
  } finally {
    clearTimeout(timeout);
  }
}

export async function account(mode: AuthMode, address: string, username: string, password: string): Promise<Profile> {
  const server = serverUrl(address);
  const login = await request<LoginResponse>(server, mode, { body: { username, password } });
  const scope = await Crypto.digestStringAsync(Crypto.CryptoDigestAlgorithm.SHA256, `${server}:${login.ledger_id}`);
  const profile: Profile = { server, scope, username: login.username, token: login.token };
  await SecureStore.setItemAsync(SESSION_KEY, JSON.stringify(profile));
  return profile;
}

export async function storedProfile(): Promise<Profile | null> {
  const value = await SecureStore.getItemAsync(SESSION_KEY);
  return value ? (JSON.parse(value) as Profile) : null;
}

export function clearProfile(): Promise<void> {
  return SecureStore.deleteItemAsync(SESSION_KEY);
}

export async function syncStatements(profile: Profile): Promise<void> {
  const { server, scope, token } = profile;
  let rejected: RequestError | undefined;
  for (const statement of await pendingStatements(scope)) {
    try {
      await request<null>(server, 'statements', { token, body: statement });
      await markSent(scope, statement.id);
    } catch (error) {
      if (!(error instanceof RequestError) || error.status !== 422) throw error;
      rejected = error;
    }
  }
  let after = await getCursor(scope);
  for (;;) {
    const page = await request<StatementPage>(server, `statements?after=${after}`, { token });
    if (page.statements.length === 0) break;
    if (page.after <= after) throw new Error('Server returned an invalid sync cursor.');
    await applyPage(scope, page.statements, page.after);
    after = page.after;
  }
  const conflicts = await conflictCount(scope);
  if (conflicts) throw new Error(`${conflicts} PDF import(s) differ from the server copy.`);
  if (rejected) throw rejected;
}
