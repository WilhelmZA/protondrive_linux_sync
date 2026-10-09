import { describe, expect, test } from 'bun:test';
import { Auth } from '../src/auth';
import { SafeLog } from '../src/log';
import { MemoryStore, fakeCrypto } from './binary-backend';
import { fixture } from './fixture';

function harness(store = new MemoryStore()) {
  const backend = fixture();
  const notifications: { method: string; params: unknown }[] = [];
  const logs: string[] = [];
  const log = new SafeLog(line => logs.push(line));
  const auth = new Auth(store, fakeCrypto, (method, params) => notifications.push({ method, params }), log, backend.fetch);
  return { backend, auth, store, notifications, logs, log };
}

describe('authentication over fake HTTP', () => {
  test('login persists only an unlocked session and sends the exact app header', async () => {
    const h = harness();
    expect(await h.auth.login('normal', 'password-secret')).toEqual({ ok: true });
    expect(h.auth.status()).toEqual({ signed_in: true, account: 'normal', keyring_locked: false });
    expect(h.store.value?.keyPassphrase).toBe('salted-password-secret');
    expect(h.backend.requests.every(r => r.version === 'external-drive-neutronsync@0.1.0-dev')).toBe(true);
    const restored = new Auth(h.store, fakeCrypto, () => {}, h.log, h.backend.fetch);
    await restored.init();
    expect(restored.status().signed_in).toBe(true);
    await h.auth.logout();
    expect(h.store.value).toBeNull();
  });
  test('2FA gates key unlock and rejects an incorrect code', async () => {
    const h = harness();
    expect(await h.auth.login('totp', 'password-secret')).toEqual({ need_2fa: true });
    expect(h.auth.status().signed_in).toBe(false);
    expect(h.store.value).toBeNull();
    await expect(h.auth.submit2fa('654321')).rejects.toMatchObject({ code: 'auth' });
    expect(await h.auth.submit2fa('123456')).toEqual({ ok: true });
    expect(h.auth.status().signed_in).toBe(true);
  });
  test('two-password mode and combined 2FA path', async () => {
    for (const username of ['two-password', 'totp-two-password']) {
      const h = harness();
      let result = await h.auth.login(username, 'password-secret');
      if ('need_2fa' in result) result = await h.auth.submit2fa('123456');
      expect(result).toEqual({ need_mailbox_password: true });
      expect(h.store.value).toBeNull();
      await expect(h.auth.submitMailbox('bad-password')).rejects.toMatchObject({ code: 'auth' });
      expect(await h.auth.submitMailbox('mailbox-secret')).toEqual({ ok: true });
      expect(h.store.value?.keyPassphrase).toBe('salted-mailbox-secret');
    }
  });
  test('human verification returns the verify URL, then login succeeds with the token', async () => {
    const h = harness();
    const first = await h.auth.login('needs-hv', 'password-secret');
    expect(first).toEqual({ need_human_verification: true, token: 'hv-token', methods: ['captcha'], url: 'https://verify.proton.me/?methods=captcha&token=hv-token' });
    expect(h.auth.status().signed_in).toBe(false);
    expect(h.store.value).toBeNull();
    expect(JSON.stringify(first)).not.toContain('password-secret');
    expect(await h.auth.login('needs-hv', 'password-secret', { token: 'hv-token', type: 'captcha' })).toEqual({ ok: true });
    expect(h.auth.status().signed_in).toBe(true);
  });
  test('passthrough hands 429 and 5xx responses to the SDK instead of throwing', async () => {
    const h = harness();
    await h.auth.login('normal', 'password-secret');
    expect((await h.auth.request('/fixture/node/limited', {}, true)).status).toBe(429);
    expect((await h.auth.request('/fixture/node/transient', {}, true)).status).toBe(503);
    await expect(h.auth.request('/fixture/node/limited')).rejects.toMatchObject({ code: 'rate_limited' });
  });
  test('bad password and bad server proof never create a session', async () => {
    const h = harness();
    await expect(h.auth.login('normal', 'bad-password')).rejects.toMatchObject({ code: 'auth' });
    await expect(h.auth.login('bad-server', 'password-secret')).rejects.toMatchObject({ code: 'auth' });
    expect(h.store.value).toBeNull();
    expect(h.auth.status().signed_in).toBe(false);
  });
  test('32 concurrent expiry requests perform one refresh and persist before retries', async () => {
    class ObservedStore extends MemoryStore {
      persisted = false;
      async save(value: Parameters<MemoryStore['save']>[0]) {
        if (value.accessToken === 'rotated-access') { await Bun.sleep(25); this.persisted = true; }
        return super.save(value);
      }
    }
    const store = new ObservedStore();
    const h = harness(store);
    await h.auth.login('normal', 'password-secret');
    h.auth.session!.expiresAt = 0;
    await Promise.all(Array.from({ length: 32 }, async () => {
      await h.auth.json('/fixture/root');
      expect(store.persisted).toBe(true);
      expect(store.value?.refreshToken).toBe('rotated-refresh');
    }));
    expect(h.backend.refreshes).toBe(1);
    expect(h.backend.requests.filter(r => r.path === '/fixture/root').every(r => r.token === 'Bearer rotated-access')).toBe(true);
  });
  test('concurrent 401 responses share one refresh, including late responses', async () => {
    const h = harness();
    await h.auth.login('normal', 'password-secret');
    h.backend.expireAccess();
    await Promise.all(Array.from({ length: 16 }, () => h.auth.json('/fixture/root')));
    expect(h.backend.refreshes).toBe(1);
  });
  test('invalid-grant signs out once and clears persistence', async () => {
    const h = harness();
    await h.auth.login('normal', 'password-secret');
    h.backend.failRefresh();
    h.auth.session!.expiresAt = 0;
    const results = await Promise.allSettled(Array.from({ length: 12 }, () => h.auth.json('/fixture/root')));
    expect(results.every(r => r.status === 'rejected')).toBe(true);
    expect(h.backend.refreshes).toBe(1);
    expect(h.notifications.filter(n => n.method === 'auth.signed_out')).toHaveLength(1);
    expect(h.store.value).toBeNull();
    expect(h.auth.status().signed_in).toBe(false);
  });
  test('a transient refresh failure keeps the session, including after a 401', async () => {
    const h = harness();
    await h.auth.login('normal', 'password-secret');
    h.backend.refreshFailure(503);
    h.auth.session!.expiresAt = 0;
    await expect(h.auth.json('/fixture/root')).rejects.toMatchObject({ code: 'transient' });
    expect(h.auth.status().signed_in).toBe(true);
    expect(h.notifications).toHaveLength(0);
    h.auth.session!.expiresAt = Date.now() + 3_600_000;
    h.backend.expireAccess();
    await expect(h.auth.json('/fixture/root')).rejects.toMatchObject({ code: 'transient' });
    expect(h.notifications.filter(n => n.method === 'auth.signed_out')).toHaveLength(0);
    expect(h.store.value).not.toBeNull();
    expect(h.auth.status().signed_in).toBe(true);
  });
  test('a missed startup load is recovered from the store, and a failed login does not wipe it', async () => {
    const h = harness();
    await h.auth.login('normal', 'password-secret');
    const saved = h.store.value;
    h.auth.session = null;
    expect(h.auth.status().signed_in).toBe(false);
    await h.auth.rehydrate();
    expect(h.auth.status()).toEqual({ signed_in: true, account: 'normal', keyring_locked: false });
    await expect(h.auth.login('normal', 'bad-password')).rejects.toMatchObject({ code: 'auth' });
    expect(h.store.value).toEqual(saved);
    h.auth.session = null;
    (h.auth as unknown as { pending: null }).pending = null;
    await h.auth.rehydrate();
    expect(h.auth.status().signed_in).toBe(true);
  });
  test('logging boundary suppresses arbitrary strings, objects and error stacks', async () => {
    const h = harness();
    await h.auth.login('normal', 'password-secret');
    const secrets = ['password-secret', '123456', 'access-secret', 'refresh-secret', 'salted-password-secret', 'mailbox-secret'];
    for (const secret of secrets) {
      h.log.write('error', `future careless caller: ${secret}`, { password: secret });
      h.log.write(secret, new Error(secret));
    }
    h.log.write('info', 'Sidecar ready', secrets);
    const captured = h.logs.join('\n') + JSON.stringify(h.notifications);
    for (const secret of secrets) expect(captured).not.toContain(secret);
    expect(captured).toContain('Diagnostic suppressed');
  });
});
