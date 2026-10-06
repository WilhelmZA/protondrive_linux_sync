import { expect, test } from 'bun:test';
import { migrateSession, parseSession, type SessionStore } from '../src/store';
import { MemoryStore } from './binary-backend';

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
