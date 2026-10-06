import { expect, test } from 'bun:test';
import { migrateSession, parseSession, type SessionStore } from '../src/store';
import { MemoryStore, fakeCrypto } from './binary-backend';
import { Auth } from '../src/auth';
import { Fault } from '../src/errors';
import { SafeLog } from '../src/log';

const session = { uid: 'uid', account: 'test', accessToken: 'access', refreshToken: 'refresh', keyPassphrase: 'key', expiresAt: 123 };

test('keyring migration verifies readback before clearing old item and is idempotent', async () => {
  const old = new MemoryStore(), current = new MemoryStore();
  await old.save(session);
  expect(await migrateSession(current, old)).toEqual(session);
  expect(await old.load()).toBeNull();
  expect(await migrateSession(current, old)).toEqual(session);
  await old.save({ ...session, accessToken: 'stale' });
  expect(await migrateSession(current, old)).toEqual(session);
  expect(await old.load()).toBeNull();
});

test('keyring migration leaves old item intact when write or readback fails', async () => {
  for (const failure of ['write', 'readback', 'invalid', 'different']) {
    const old = new MemoryStore();
    await old.save(session);
    let written = false;
    const current: SessionStore = {
      async load() {
        if (!written) return null;
        if (failure === 'readback') throw new Error();
        if (failure === 'invalid') return parseSession('{}');
        return { ...session, accessToken: 'different' };
      },
      async save() { if (failure === 'write') throw new Error(); written = true; },
      async clear() {},
    };
    await expect(migrateSession(current, old)).rejects.toThrow();
    expect(await old.load()).toEqual(session);
  }
});

test('Auth initialization fails instead of reporting signed out, then retries migration without a login', async () => {
  for (const failure of ['write', 'readback', 'invalid']) {
    for (const restart of [false, true]) {
      const old = new MemoryStore();
      await old.save(session);
      let broken = true;
      let stored: string | null = null;
      const current: SessionStore = {
        async load() {
          if (stored === null) return null;
          if (broken && failure === 'readback') throw new Fault('fatal');
          return parseSession(stored);
        },
        async save(value) {
          if (broken && failure === 'write') throw new Fault('fatal');
          stored = broken && failure === 'invalid' ? '{}' : JSON.stringify(value);
        },
        async clear() { stored = null; },
      };
      const store = { load: () => migrateSession(current, old), save: current.save, clear: current.clear };
      const notices: string[] = [], logs: string[] = [];
      const create = () => new Auth(store, fakeCrypto, method => notices.push(method), new SafeLog(line => logs.push(line)));
      let auth = create();
      await expect(auth.init()).rejects.toMatchObject({ code: 'fatal' });
      expect(() => auth.status()).toThrow('Operation failed');
      expect(await old.load()).toEqual(session);
      expect(notices).toEqual([]);
      expect(logs.join('\n')).not.toContain(session.refreshToken);
      broken = false;
      if (restart) auth = create();
      await auth.init();
      expect(auth.status()).toEqual({ signed_in: true, account: 'test' });
      expect(await current.load()).toEqual(session);
      expect(await old.load()).toBeNull();
      expect(notices).toEqual([]);
    }
  }
});

test('storage read failure does not overwrite a potentially newer target or clear legacy persistence', async () => {
  const old = new MemoryStore();
  await old.save(session);
  let writes = 0;
  const unreadable: SessionStore = {
    async load() { throw new Fault('fatal'); },
    async save() { writes++; },
    async clear() { throw new Error('unexpected clear'); },
  };
  await expect(migrateSession(unreadable, old)).rejects.toMatchObject({ code: 'fatal' });
  expect(writes).toBe(0);
  expect(await old.load()).toEqual(session);
  const invalid = { ...unreadable, async load() { return parseSession('{}'); } };
  await expect(migrateSession(invalid, new MemoryStore())).rejects.toMatchObject({ code: 'fatal' });
  expect(writes).toBe(0);
});
