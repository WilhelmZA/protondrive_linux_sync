import { expect, test } from 'bun:test';
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomBytes } from 'node:crypto';
import { Fault } from '../src/errors';
import {
  FileSessionStore, SecretServiceStore, SessionKeyError, parseSessionKey, sessionKeyFromEnv, sessionKeyMessage,
  storeFromEnv, tightenedMessage, undecryptableMessage,
} from '../src/store';
import { binary } from './compiled';

const session = {
  uid: 'synthetic-uid', account: 'file-store-test', accessToken: 'synthetic-access-token-1234',
  refreshToken: 'synthetic-refresh-token-5678', keyPassphrase: 'synthetic-key-passphrase-9012', expiresAt: 1234567890123,
};

function scratch() {
  const dir = mkdtempSync(join(tmpdir(), 'ns-file-store-'));
  return { dir, path: join(dir, 'session.enc'), done: () => rmSync(dir, { recursive: true, force: true }) };
}

test('save then load round-trips; the file is 0600 JSON with no secret in it', async () => {
  const s = scratch();
  try {
    const key = randomBytes(32);
    const store = new FileSessionStore(s.path, key);
    await store.save(session);
    expect(await store.load()).toEqual(session);
    expect(statSync(s.path).mode & 0o777).toBe(0o600);
    const raw = readFileSync(s.path, 'utf8');
    const file = JSON.parse(raw);
    expect(Object.keys(file).sort()).toEqual(['ct', 'iv', 'v']);
    expect(file.v).toBe(1);
    expect(Buffer.from(file.iv, 'base64').length).toBe(12);
    for (const secret of [session.accessToken, session.refreshToken, session.keyPassphrase]) {
      for (const form of [secret, Buffer.from(secret).toString('base64'), Buffer.from(secret).toString('base64url'), Buffer.from(secret).toString('hex')]) {
        expect(raw).not.toContain(form);
      }
    }
    expect(raw).not.toContain(key.toString('base64'));
    // No temp file is left beside it.
    expect(require('node:fs').readdirSync(s.dir)).toEqual(['session.enc']);

    await store.save(session);
    const second = JSON.parse(readFileSync(s.path, 'utf8'));
    expect(second.iv).not.toBe(file.iv);
    expect(second.ct).not.toBe(file.ct);
    expect(await store.load()).toEqual(session);
  } finally { s.done(); }
});

test('a wrong key, a truncated or an edited file is fatal and leaves the file byte-identical', async () => {
  const s = scratch();
  try {
    const notes: string[] = [];
    const store = new FileSessionStore(s.path, randomBytes(32), m => notes.push(m));
    await store.save(session);
    const good = readFileSync(s.path);
    const wrong = new FileSessionStore(s.path, randomBytes(32), m => notes.push(m));
    const failure = await wrong.load().catch(error => error);
    expect(failure).toBeInstanceOf(Fault);
    expect(failure.code).toBe('fatal');
    expect(readFileSync(s.path)).toEqual(good);
    expect(notes).toContain(undecryptableMessage);

    const file = JSON.parse(good.toString());
    const ct = Buffer.from(file.ct, 'base64');
    ct[0] = ct[0]! ^ 1;
    const variants = [
      good.subarray(0, good.length - 10),
      Buffer.from(JSON.stringify({ ...file, ct: ct.toString('base64') })),
      Buffer.from(JSON.stringify({ ...file, v: 2 })),
      Buffer.from('not json at all'),
    ];
    for (const bytes of variants) {
      writeFileSync(s.path, bytes, { mode: 0o600 });
      await expect(store.load()).rejects.toMatchObject({ code: 'fatal' });
      expect(readFileSync(s.path)).toEqual(bytes);
    }
  } finally { s.done(); }
});

test('a missing file is no session; clear removes it and tolerates absence; loose modes are tightened', async () => {
  const s = scratch();
  try {
    const notes: string[] = [];
    const store = new FileSessionStore(s.path, randomBytes(32), m => notes.push(m));
    expect(await store.load()).toBeNull();
    await store.clear();
    await store.save(session);
    chmodSync(s.path, 0o644);
    expect(await store.load()).toEqual(session);
    expect(statSync(s.path).mode & 0o777).toBe(0o600);
    expect(notes).toEqual([tightenedMessage]);
    await store.clear();
    expect(existsSync(s.path)).toBe(false);
    expect(await store.load()).toBeNull();
    expect(await store.isLocked()).toBe(false);
  } finally { s.done(); }
});

test('keys are 32 bytes as base64 or hex, from the environment or a file', () => {
  const key = randomBytes(32);
  expect(parseSessionKey(`  ${key.toString('base64')}\n`)).toEqual(new Uint8Array(key));
  expect(parseSessionKey(key.toString('hex').toUpperCase())).toEqual(new Uint8Array(key));
  for (const bad of ['', 'short', randomBytes(31).toString('base64'), randomBytes(33).toString('hex'), 'z'.repeat(64)]) {
    expect(() => parseSessionKey(bad)).toThrow(SessionKeyError);
  }
  const s = scratch();
  try {
    const file = join(s.dir, 'session.key');
    writeFileSync(file, key.toString('hex') + '\n');
    expect(sessionKeyFromEnv({ NEUTRONSYNC_SESSION_KEY_FILE: file })).toEqual(new Uint8Array(key));
    expect(() => sessionKeyFromEnv({ NEUTRONSYNC_SESSION_KEY_FILE: join(s.dir, 'absent') })).toThrow(sessionKeyMessage);
    expect(() => sessionKeyFromEnv({})).toThrow(sessionKeyMessage);
    const env = { NEUTRONSYNC_SESSION_STORE: 'file', NEUTRONSYNC_SESSION_KEY: key.toString('base64'), NEUTRONSYNC_SESSION_FILE: join(s.dir, 'session.enc') };
    expect(storeFromEnv(env, () => {})).toBeInstanceOf(FileSessionStore);
    expect(storeFromEnv({}, () => {})).toBeInstanceOf(SecretServiceStore);
    expect(storeFromEnv({ NEUTRONSYNC_SESSION_STORE: 'keyring' }, () => {})).toBeInstanceOf(SecretServiceStore);
    expect(() => storeFromEnv({ NEUTRONSYNC_SESSION_STORE: 'vault' }, () => {})).toThrow('Unknown session store');
  } finally { s.done(); }
});

test('the compiled sidecar with the file store and no usable key exits before creating its socket', async () => {
  const s = scratch();
  try {
    const socket = join(s.dir, 'run', 'session.sock');
    const keyFile = join(s.dir, 'session.key');
    writeFileSync(keyFile, randomBytes(16).toString('hex'));
    const cases: Record<string, string>[] = [
      {},
      { NEUTRONSYNC_SESSION_KEY: randomBytes(31).toString('base64') },
      { NEUTRONSYNC_SESSION_KEY_FILE: join(s.dir, 'absent.key') },
      { NEUTRONSYNC_SESSION_KEY_FILE: keyFile },
    ];
    for (const extra of cases) {
      const env: Record<string, string> = { ...process.env as Record<string, string>, NEUTRONSYNC_SESSION_STORE: 'file', NEUTRONSYNC_SESSION_FILE: s.path, ...extra };
      delete env.NEUTRONSYNC_SESSION_KEY;
      if (extra.NEUTRONSYNC_SESSION_KEY) env.NEUTRONSYNC_SESSION_KEY = extra.NEUTRONSYNC_SESSION_KEY;
      const child = Bun.spawn([binary, '--socket', socket], { env, stdin: 'ignore', stdout: 'pipe', stderr: 'pipe' });
      const timer = setTimeout(() => child.kill('SIGKILL'), 10000);
      const [status, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()]);
      clearTimeout(timer);
      expect(status).not.toBe(0);
      expect(stderr).toContain(sessionKeyMessage);
      expect(existsSync(socket)).toBe(false);
      expect(existsSync(socket + '.lock')).toBe(false);
      expect(existsSync(s.path)).toBe(false);
    }

    // With a key it starts over stdio, signed out, and reads a saved session.
    const key = randomBytes(32);
    const store = new FileSessionStore(s.path, key);
    const env = { ...process.env as Record<string, string>, NEUTRONSYNC_SESSION_STORE: 'file', NEUTRONSYNC_SESSION_FILE: s.path, NEUTRONSYNC_SESSION_KEY: key.toString('base64') };
    const status = async () => {
      const child = Bun.spawn([binary], { env, stdin: new Blob(['{"jsonrpc":"2.0","id":1,"method":"auth.status","params":{}}\n']), stdout: 'pipe', stderr: 'pipe' });
      const [code, out] = await Promise.all([child.exited, new Response(child.stdout).text()]);
      return { code, result: out.trim().split('\n').map(l => JSON.parse(l)).find(m => m.id === 1)?.result };
    };
    expect(await status()).toEqual({ code: 0, result: { signed_in: false, account: null, keyring_locked: false } });
    await store.save(session);
    expect(await status()).toEqual({ code: 0, result: { signed_in: true, account: 'file-store-test', keyring_locked: false } });
    // A wrong key stops start-up and keeps the file.
    const before = readFileSync(s.path);
    env.NEUTRONSYNC_SESSION_KEY = randomBytes(32).toString('base64');
    const child = Bun.spawn([binary], { env, stdin: new Blob(['{"jsonrpc":"2.0","id":1,"method":"auth.status","params":{}}\n']), stdout: 'pipe', stderr: 'pipe' });
    const [code, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()]);
    expect(code).not.toBe(0);
    expect(stderr).toContain(undecryptableMessage);
    expect(stderr).not.toContain(session.accessToken);
    expect(readFileSync(s.path)).toEqual(before);
  } finally { s.done(); }
}, 60000);
