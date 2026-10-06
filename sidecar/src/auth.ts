import { timingSafeEqual } from 'node:crypto';
import { getSrp, computeKeyPassword } from '@protontech/crypto/srp';
import { version } from '../package.json';
import { Fault, httpFault } from './errors';
import type { Notify, SafeLog } from './log';
import type { Session, SessionStore } from './store';

export const appVersion = `external-drive-neutronsync@${version}-dev`;
export type Fetch = (url: string, init: RequestInit) => Promise<Response>;
export interface AuthCrypto {
  proof: typeof getSrp;
  keyPassword: typeof computeKeyPassword;
  unlock: (auth: Auth, passphrase: string) => Promise<void>;
}
type Pending = { session: Session; password: string; twoPassword: boolean; totp: boolean };
type LoginResult = { ok: true } | { need_2fa: true } | { need_mailbox_password: true };

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
    if (init.body) headers.set('Content-Type', 'application/json');
    if (session) {
      headers.set('x-pm-uid', session.uid);
      headers.set('Authorization', `Bearer ${session.accessToken}`);
    }
    try {
      return await this.fetcher(url.href, { ...init, headers, redirect: 'error', signal: init.signal ?? AbortSignal.timeout(30_000) });
    } catch { throw new Fault('transient'); }
  }

  private async decode(response: Response): Promise<any> {
    if (!response.ok) throw httpFault(response.status, response.headers.get('retry-after'));
    let data: any;
    try { data = await response.json(); } catch { throw new Fault('fatal'); }
    // Do not expose server-provided Error/Details, which can echo credentials.
    if (data.Code !== undefined && data.Code !== 1000 && data.Code !== 1001) {
      if (data.Code === 9001) throw new Fault('auth');
      throw new Fault('fatal');
    }
    return data;
  }

  async json(path: string, init: RequestInit = {}, authenticated = true): Promise<any> {
    return this.decode(authenticated ? await this.request(path, init) : await this.raw(path, init));
  }

  async request(path: string, init: RequestInit = {}): Promise<Response> {
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
    if (!response.ok) {
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

  async login(username: string, password: string): Promise<LoginResult> {
    await this.logout();
    const info = await this.json('/auth/v4/info', { method: 'POST', body: JSON.stringify({ Username: username, Intent: 'Proton' }) }, false);
    let proof: Awaited<ReturnType<typeof getSrp>>;
    try { proof = await this.crypto.proof(info, { username, password }); }
    catch { throw new Fault('auth'); }
    const data = await this.json('/auth/v4', {
      method: 'POST', body: JSON.stringify({ Username: username, ClientEphemeral: proof.clientEphemeral, ClientProof: proof.clientProof, SRPSession: info.SRPSession }),
    }, false);
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
