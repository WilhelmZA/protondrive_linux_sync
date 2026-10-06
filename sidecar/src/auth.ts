import { timingSafeEqual } from 'node:crypto';
import { getSrp, computeKeyPassword } from '@protontech/crypto/srp';
import { version } from '../package.json';
import { Fault, httpFault } from './errors';
import type { Notify, SafeLog } from './log';
import type { Session, SessionStore } from './store';

// Opt-in diagnostics: path, HTTP status and numeric API code only. Never message text or bodies.
const apiDebug = process.env.NEUTRONSYNC_DRIVE_DEBUG === '1';
function debugApi(response: Response, code: unknown) {
  if (!apiDebug) return;
  let path = '?';
  try { path = new URL(response.url).pathname; } catch {}
  process.stderr.write(JSON.stringify({ debug: 'api', path, status: response.status, code: typeof code === 'number' ? code : null }) + '\n');
}

export const appVersion = `external-drive-neutronsync@${version}-dev`;
export type Fetch = (url: string, init: RequestInit) => Promise<Response>;
export interface AuthCrypto {
  proof: typeof getSrp;
  keyPassword: typeof computeKeyPassword;
  unlock: (auth: Auth, passphrase: string) => Promise<void>;
}
type Pending = { session: Session; password: string; twoPassword: boolean; totp: boolean };
type HumanVerification = { need_human_verification: true; url: string; token: string; methods: string[] };
type LoginResult = { ok: true } | { need_2fa: true } | { need_mailbox_password: true } | HumanVerification;
export type HvType = 'captcha' | 'email' | 'sms';
export const hvTypes: readonly string[] = ['captcha', 'email', 'sms'];

export class Auth {
  session: Session | null = null;
  private pending: Pending | null = null;
  private refreshFlight: Promise<void> | null = null;
  private generation = 0;
  private initialized = false;
  private retryAt = 0;
  onClear: () => void = () => {};

  constructor(
    private store: SessionStore,
    private crypto: AuthCrypto,
    private notify: Notify,
    private log: SafeLog,
    private fetcher: Fetch = fetch,
    private base = 'https://drive-api.proton.me',
  ) {}

  async init() {
    if (this.initialized) return;
    this.initialized = true;
    try { this.session = await this.store.load(); }
    catch { this.log.write('warn', 'Session storage unavailable'); }
  }

  private async raw(path: string, init: RequestInit, session?: Session | null): Promise<Response> {
    const url = new URL(path, this.base);
    if (url.origin !== new URL(this.base).origin) throw new Fault('fatal');
    const headers = new Headers(init.headers);
    headers.set('x-pm-appversion', appVersion);
    headers.set('Accept', 'application/vnd.protonmail.v1+json');
    if (init.body && !(init.body instanceof FormData)) headers.set('Content-Type', 'application/json');
    if (session) {
      headers.set('x-pm-uid', session.uid);
      headers.set('Authorization', `Bearer ${session.accessToken}`);
    }
    try {
      return await this.fetcher(url.href, { ...init, headers, redirect: 'error', signal: init.signal ?? AbortSignal.timeout(30_000) });
    } catch { throw new Fault('transient'); }
  }

  private async decode(response: Response): Promise<any> {
    if (!response.ok) {
      if (apiDebug) {
        const body = await response.clone().json().catch(() => ({})) as { Code?: unknown };
        debugApi(response, body.Code);
      }
      throw httpFault(response.status, response.headers.get('retry-after'));
    }
    let data: any;
    try { data = await response.json(); } catch { throw new Fault('fatal'); }
    // Do not expose server-provided Error/Details, which can echo credentials.
    if (data.Code !== undefined && data.Code !== 1000 && data.Code !== 1001) {
      debugApi(response, data.Code);
      if (data.Code === 9001) throw new Fault('auth');
      throw new Fault('fatal');
    }
    return data;
  }

  async json(path: string, init: RequestInit = {}, authenticated = true): Promise<any> {
    return this.decode(authenticated ? await this.request(path, init) : await this.raw(path, init));
  }

  // Storage hosts use SDK storage tokens, never our account session.
  async blob(url: string, init: RequestInit): Promise<Response> {
    const headers = new Headers(init.headers);
    headers.delete('Authorization');
    headers.delete('x-pm-uid');
    headers.set('x-pm-appversion', appVersion);
    if (new URL(url).origin === new URL(this.base).origin) {
      const session = this.requireSession();
      headers.set('Authorization', `Bearer ${session.accessToken}`);
      headers.set('x-pm-uid', session.uid);
    }
    try { return await this.fetcher(url, { ...init, headers, redirect: 'error' }); }
    catch { throw new Fault('transient'); }
  }

  /**
   * Authenticated request with single-flight 401 refresh.
   * `passthrough` returns non-OK responses (429, 5xx, API errors) unchanged, for the SDK:
   * its apiService does its own retry-after, 5xx retry and error-code parsing, and cannot
   * do any of that if this layer throws first.
   */
  async request(path: string, init: RequestInit = {}, passthrough = false): Promise<Response> {
    if (this.retryAt > Date.now()) await Bun.sleep(this.retryAt - Date.now());
    if (!this.session) throw new Fault('auth');
    const session = this.session;
    if (session.expiresAt <= Date.now() + 30_000) await this.refresh(session.accessToken);
    if (!this.session) throw new Fault('auth');
    const used = this.session.accessToken;
    let response = await this.raw(path, init, this.session);
    if (response.status === 401) {
      try { await this.refresh(used); }
      catch { await this.signOut(); throw new Fault('auth'); }
      if (!this.session) throw new Fault('auth');
      response = await this.raw(path, init, this.session);
      if (response.status === 401) { await this.signOut(); throw new Fault('auth'); }
    }
    if (!response.ok && !passthrough) {
      const fault = httpFault(response.status, response.headers.get('retry-after'));
      if (fault.code === 'rate_limited') this.retryAt = Date.now() + (fault.retry_after ?? 60) * 1000;
      throw fault;
    }
    return response;
  }

  private async refresh(usedToken: string) {
    if (this.refreshFlight) return this.refreshFlight;
    if (!this.session) throw new Fault('auth');
    if (this.session.accessToken !== usedToken) return;
    const current = this.session;
    const generation = this.generation;
    this.refreshFlight = (async () => {
      let rotated = false;
      try {
        const response = await this.raw('/auth/v4/refresh', {
          method: 'POST', body: JSON.stringify({ UID: current.uid, RefreshToken: current.refreshToken, ResponseType: 'token', GrantType: 'refresh_token', RedirectURI: 'https://proton.me' }),
        }, current);
        if (response.status === 400) {
          const error = await response.clone().json().catch(() => ({})) as { error?: string };
          if (error.error === 'invalid_grant') throw new Fault('auth');
        }
        const data = await this.decode(response);
        if (!data.AccessToken || !data.RefreshToken) throw new Fault('auth');
        rotated = true;
        const next = { ...current, accessToken: data.AccessToken, refreshToken: data.RefreshToken, expiresAt: Date.now() + (data.ExpiresIn ?? 3600) * 1000 };
        if (generation !== this.generation) throw new Fault('auth');
        await this.store.save(next);
        if (generation !== this.generation) { await this.store.clear(); throw new Fault('auth'); }
        this.session = next;
      } catch (error) {
        if (generation === this.generation && (rotated || (error instanceof Fault && error.code === 'auth'))) {
          await this.signOut();
          throw new Fault('auth');
        }
        throw error;
      }
    })().finally(() => { this.refreshFlight = null; });
    return this.refreshFlight;
  }

  async login(username: string, password: string, hv?: { token: string; type: HvType }): Promise<LoginResult> {
    await this.logout();
    const info = await this.json('/auth/v4/info', { method: 'POST', body: JSON.stringify({ Username: username, Intent: 'Proton' }) }, false);
    let proof: Awaited<ReturnType<typeof getSrp>>;
    try { proof = await this.crypto.proof(info, { username, password }); }
    catch { throw new Fault('auth'); }
    const headers: Record<string, string> = hv ? { 'x-pm-human-verification-token': hv.token, 'x-pm-human-verification-token-type': hv.type } : {};
    const response = await this.raw('/auth/v4', {
      method: 'POST', headers, body: JSON.stringify({ Username: username, ClientEphemeral: proof.clientEphemeral, ClientProof: proof.clientProof, SRPSession: info.SRPSession }),
    });
    // 9001: Proton wants human verification. Surface only the token and methods, never other Details.
    if (response.status === 422) {
      const body = await response.clone().json().catch(() => ({})) as { Code?: number; Details?: { HumanVerificationToken?: unknown; HumanVerificationMethods?: unknown } };
      const token = body.Details?.HumanVerificationToken;
      if (body.Code === 9001 && typeof token === 'string' && token) {
        const offered = Array.isArray(body.Details?.HumanVerificationMethods) ? body.Details.HumanVerificationMethods.filter((m): m is string => typeof m === 'string' && hvTypes.includes(m)) : [];
        const methods = offered.length ? offered : ['captcha'];
        return { need_human_verification: true, token, methods, url: `https://verify.proton.me/?methods=${methods.join(',')}&token=${encodeURIComponent(token)}` };
      }
    }
    const data = await this.decode(response);
    const expected = Buffer.from(proof.expectedServerProof, 'base64');
    const actual = Buffer.from(data.ServerProof ?? '', 'base64');
    if (!expected.length || expected.length !== actual.length || !timingSafeEqual(expected, actual) || !data.UID || !data.AccessToken || !data.RefreshToken) throw new Fault('auth');
    this.pending = {
      session: { uid: data.UID, accessToken: data.AccessToken, refreshToken: data.RefreshToken, keyPassphrase: '', account: username, expiresAt: Date.now() + (data.ExpiresIn ?? 3600) * 1000 },
      password, twoPassword: data.PasswordMode === 2, totp: !!(data['2FA']?.Enabled & 1),
    };
    if (data['2FA']?.Enabled && !this.pending.totp) { this.pending = null; throw new Fault('auth'); }
    if (this.pending.totp) return { need_2fa: true };
    return this.afterFactors();
  }

  async submit2fa(code: string): Promise<LoginResult> {
    if (!this.pending?.totp) throw new Fault('auth');
    await this.decode(await this.raw('/auth/v4/2fa', { method: 'POST', body: JSON.stringify({ TwoFactorCode: code }) }, this.pending.session));
    this.pending.totp = false;
    return this.afterFactors();
  }
  private async afterFactors(): Promise<LoginResult> {
    if (!this.pending) throw new Fault('auth');
    if (this.pending.twoPassword) { this.pending.password = ''; return { need_mailbox_password: true }; }
    return this.unlock(this.pending.password);
  }
  async submitMailbox(password: string): Promise<{ ok: true }> {
    if (!this.pending?.twoPassword || this.pending.totp) throw new Fault('auth');
    return this.unlock(password);
  }
  private async unlock(password: string): Promise<{ ok: true }> {
    const pending = this.pending!;
    // Temporary session permits account reads but status stays signed out until keys unlock.
    this.session = pending.session;
    try {
      const [{ User }, { KeySalts }] = await Promise.all([this.json('/core/v4/users'), this.json('/core/v4/keys/salts')]);
      const primary = User.Keys.find((key: any) => key.Primary === 1) ?? User.Keys[0];
      const salt = KeySalts.find((key: any) => key.ID === primary?.ID)?.KeySalt;
      if (!salt) throw new Fault('auth');
      const keyPassphrase = await this.crypto.keyPassword(password, salt);
      await this.crypto.unlock(this, keyPassphrase);
      const ready = { ...this.session!, keyPassphrase };
      await this.store.save(ready);
      this.session = ready;
      this.pending = null;
      return { ok: true };
    } catch (error) {
      this.session = null;
      this.onClear();
      if (error instanceof Fault) throw error;
      throw new Fault('auth');
    }
  }
  status() { return { signed_in: !!this.session?.keyPassphrase && !this.pending, account: this.session?.keyPassphrase && !this.pending ? this.session.account : null }; }
  requireSession(): Session {
    if (!this.status().signed_in) throw new Fault('auth');
    return this.session!;
  }
  private async signOut() {
    const hadSession = !!this.session || !!this.pending;
    this.generation++;
    this.session = null;
    this.pending = null;
    this.onClear();
    if (hadSession) this.notify('auth.signed_out', {});
    try { await this.store.clear(); } catch { this.log.write('warn', 'Session storage unavailable'); }
  }
  async logout(): Promise<{ ok: true }> {
    this.generation++;
    this.session = null;
    this.pending = null;
    this.onClear();
    // Wait for an existing refresh to finish before clearing its persisted result.
    if (this.refreshFlight) await this.refreshFlight.catch(() => {});
    await this.store.clear();
    return { ok: true };
  }
}
