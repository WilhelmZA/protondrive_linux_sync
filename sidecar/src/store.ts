import { Fault } from './errors';
import legacySession from '../legacy-session.json';
import { chmodSync, closeSync, constants, fsyncSync, openSync, readFileSync, renameSync, statSync, unlinkSync, writeSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import { basename, dirname, join } from 'node:path';

export interface Session {
  uid: string;
  accessToken: string;
  refreshToken: string;
  keyPassphrase: string;
  account: string;
  expiresAt: number;
}
export interface SessionStore {
  load(): Promise<Session | null>;
  save(session: Session): Promise<void>;
  clear(): Promise<void>;
  /** True when the Secret Service collection is locked. Absent on test stores. */
  isLocked?(): Promise<boolean>;
}

/** The keyring is locked. This is not "no session" and must not be treated as a sign-out. */
export class KeyringLocked extends Error {
  readonly code = 'keyring_locked';
}

export const keyringAttributes = ['application', 'neutronsync-drive', 'purpose', 'session-v1'];
export const keyringLabel = 'NeutronSync Drive session';
// Compatibility identifier only; never use it for a new session.
export const legacyAttributes = legacySession.attributes;

export class InvalidStoredSession extends Fault {
  constructor() { super('fatal'); }
}

const lockPaths = ['/org/freedesktop/secrets/aliases/default', '/org/freedesktop/secrets/collection/login'];

/** `dbus-send` prints `variant boolean true|false`; `busctl` prints `b true|false`. */
export function parseLockedReply(output: string): boolean | null {
  const match = output.match(/(?:boolean|\bb)\s+(true|false)\b/i);
  if (!match?.[1]) return null;
  return match[1].toLowerCase() === 'true';
}

// A property read does not prompt. `secret-tool lookup` does, and a locked
// collection often makes that lookup look like "no item", which is how a
// locked keyring was mistaken for a sign-out.
async function collectionIsLocked(): Promise<boolean> {
  for (const path of lockPaths) {
    try {
      const child = Bun.spawn([
        'dbus-send', '--session', '--print-reply', '--reply-timeout=3000',
        '--dest=org.freedesktop.secrets', path,
        'org.freedesktop.DBus.Properties.Get',
        'string:org.freedesktop.Secret.Collection', 'string:Locked',
      ], { stdin: 'ignore', stdout: 'pipe', stderr: 'pipe' });
      const [output, status] = await Promise.all([new Response(child.stdout).text(), child.exited]);
      if (status !== 0) continue;
      const locked = parseLockedReply(output);
      if (locked !== null) return locked;
    } catch { continue; }
  }
  return false;
}

export function parseSession(output: string): Session {
  try {
    const value = JSON.parse(output);
    if (!['uid', 'accessToken', 'refreshToken', 'keyPassphrase', 'account'].every(k => typeof value[k] === 'string' && value[k]) || !Number.isFinite(value.expiresAt)) throw 0;
    return value;
  } catch { throw new InvalidStoredSession(); }
}

export async function migrateSession(current: SessionStore, old: SessionStore): Promise<Session | null> {
  let present: Session | null;
  let invalidTarget = false;
  try { present = await current.load(); }
  catch (error) {
    // A failed migration may leave malformed target data. Only a successfully
    // read, invalid item permits repair from the legacy copy. A storage read
    // failure must not overwrite an unreadable but potentially newer session.
    if (!(error instanceof InvalidStoredSession)) throw error;
    invalidTarget = true;
    present = null;
  }
  if (present) { await old.clear(); return present; }
  const previous = await old.load();
  if (!previous) {
    if (invalidTarget) throw new Fault('fatal');
    return null;
  }
  await current.save(previous);
  const verified = await current.load();
  if (!verified || JSON.stringify(verified) !== JSON.stringify(previous)) throw new Fault('fatal');
  await old.clear();
  return verified;
}

export class SecretServiceStore implements SessionStore {
  constructor(private attributes = keyringAttributes, private label = keyringLabel, private migrate = true) {}
  private async run(args: string[], secret?: string) {
    try {
      const child = Bun.spawn(['secret-tool', ...args], {
        stdin: secret === undefined ? 'ignore' : new Blob([secret]), stdout: 'pipe', stderr: 'pipe',
      });
      const [output, diagnostic, status] = await Promise.all([
        new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited,
      ]);
      return { output, status, hasDiagnostic: diagnostic.trim().length > 0 };
    } catch { throw new Fault('fatal'); }
  }
  async isLocked() { return collectionIsLocked(); }
  async load(): Promise<Session | null> {
    if (await this.isLocked()) throw new KeyringLocked();
    if (this.migrate) return migrateSession(new SecretServiceStore(this.attributes, this.label, false), new SecretServiceStore(legacyAttributes, this.label, false));
    const { output, status, hasDiagnostic } = await this.run(['lookup', ...this.attributes]);
    if (status === 1 && !output && !hasDiagnostic) return null;
    if (status !== 0) throw new Fault('fatal');
    return parseSession(output);
  }
  async save(session: Session) {
    const { status } = await this.run(['store', '--label', this.label, ...this.attributes], JSON.stringify(session));
    if (status !== 0) throw new Fault('fatal');
  }
  async clear() {
    const { status } = await this.run(['clear', ...this.attributes]);
    if (status !== 0 && status !== 1) throw new Fault('fatal');
  }
}

// ---- encrypted session file ------------------------------------------------

export const sessionKeyMessage = 'Session file store needs a 32-byte key (NEUTRONSYNC_SESSION_KEY or NEUTRONSYNC_SESSION_KEY_FILE).';
export const sessionFileMessage = 'Session file store needs NEUTRONSYNC_SESSION_FILE.';
export const sessionStoreMessage = 'Unknown session store (NEUTRONSYNC_SESSION_STORE must be keyring or file).';
export const undecryptableMessage = 'The saved session cannot be decrypted: wrong key or damaged file.';
export const tightenedMessage = 'Session file permissions tightened to 0600';
const additionalData = new TextEncoder().encode('neutronsync-session-v1');

/** The session key is missing, unreadable or not 32 bytes. */
export class SessionKeyError extends Error {
  constructor() { super(sessionKeyMessage); }
}

/** 64 hex characters or base64 of exactly 32 bytes; surrounding whitespace ignored. */
export function parseSessionKey(text: string): Uint8Array {
  const value = text.trim();
  let bytes: Uint8Array | null = null;
  if (/^[0-9a-fA-F]{64}$/.test(value)) bytes = new Uint8Array(Buffer.from(value, 'hex'));
  else if (/^[A-Za-z0-9+/]+={0,2}$/.test(value) && value.length % 4 === 0) bytes = new Uint8Array(Buffer.from(value, 'base64'));
  if (!bytes || bytes.length !== 32) throw new SessionKeyError();
  return bytes;
}

/** The key from NEUTRONSYNC_SESSION_KEY, else from the file NEUTRONSYNC_SESSION_KEY_FILE names. */
export function sessionKeyFromEnv(env: Record<string, string | undefined>): Uint8Array {
  if (env.NEUTRONSYNC_SESSION_KEY) return parseSessionKey(env.NEUTRONSYNC_SESSION_KEY);
  const file = env.NEUTRONSYNC_SESSION_KEY_FILE;
  if (!file) throw new SessionKeyError();
  let text: string;
  try { text = readFileSync(file, 'utf8'); } catch { throw new SessionKeyError(); }
  return parseSessionKey(text);
}

const toBase64 = (bytes: Uint8Array) => Buffer.from(bytes).toString('base64');
const fromBase64 = (text: unknown) => {
  if (typeof text !== 'string' || !/^[A-Za-z0-9+/]*={0,2}$/.test(text)) throw new Error();
  return new Uint8Array(Buffer.from(text, 'base64'));
};

/**
 * The session in one file, AES-256-GCM through Web Crypto: `{"v":1,"iv","ct"}`
 * with a fresh random IV on every save and `neutronsync-session-v1` as
 * additional data. For machines with no Secret Service (a container). A file
 * that does not decrypt is never overwritten or removed here: only `clear()`
 * (an explicit sign-out) deletes it, so a mistyped key cannot destroy a good
 * session.
 */
export class FileSessionStore implements SessionStore {
  private key: Promise<CryptoKey>;
  constructor(private path: string, rawKey: Uint8Array, private note: (message: string) => void = () => {}) {
    if (rawKey.length !== 32) throw new SessionKeyError();
    this.key = crypto.subtle.importKey('raw', new Uint8Array(rawKey), { name: 'AES-GCM' }, false, ['encrypt', 'decrypt']);
  }

  async isLocked() { return false; }

  async load(): Promise<Session | null> {
    let mode: number;
    try { mode = statSync(this.path).mode; }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code === 'ENOENT') return null;
      throw new Fault('fatal');
    }
    if (mode & 0o077) {
      try { chmodSync(this.path, 0o600); this.note(tightenedMessage); }
      catch { throw new Fault('fatal'); }
    }
    let text: string;
    try { text = readFileSync(this.path, 'utf8'); } catch { throw new Fault('fatal'); }
    let plain: string;
    try {
      const file = JSON.parse(text);
      if (file?.v !== 1) throw new Error();
      const iv = fromBase64(file.iv);
      if (iv.length !== 12) throw new Error();
      const bytes = await crypto.subtle.decrypt({ name: 'AES-GCM', iv, additionalData }, await this.key, fromBase64(file.ct));
      plain = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    } catch {
      this.note(undecryptableMessage);
      throw new Fault('fatal');
    }
    return parseSession(plain);
  }

  async save(session: Session) {
    const iv = randomBytes(12);
    const ct = new Uint8Array(await crypto.subtle.encrypt(
      { name: 'AES-GCM', iv, additionalData }, await this.key, new TextEncoder().encode(JSON.stringify(session)),
    ));
    const body = JSON.stringify({ v: 1, iv: toBase64(iv), ct: toBase64(ct) });
    const temp = join(dirname(this.path), `.${basename(this.path)}.${process.pid}.${toBase64(randomBytes(6)).replace(/[+/=]/g, '_')}.tmp`);
    let fd: number | undefined;
    try {
      fd = openSync(temp, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY | constants.O_NOFOLLOW, 0o600);
      writeSync(fd, body);
      fsyncSync(fd);
      closeSync(fd); fd = undefined;
      renameSync(temp, this.path);
    } catch {
      if (fd !== undefined) try { closeSync(fd); } catch {}
      try { unlinkSync(temp); } catch {}
      throw new Fault('fatal');
    }
  }

  async clear() {
    try { unlinkSync(this.path); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw new Fault('fatal'); }
  }
}

/**
 * Which store the sidecar uses, from NEUTRONSYNC_SESSION_STORE: the keyring
 * (unset or `keyring`) or the encrypted file (`file`). Throws with a message
 * fit for the log when the file store cannot be used.
 */
export function storeFromEnv(env: Record<string, string | undefined>, note: (message: string) => void): SessionStore {
  const kind = env.NEUTRONSYNC_SESSION_STORE ?? 'keyring';
  if (kind === 'keyring' || kind === '') return new SecretServiceStore();
  if (kind !== 'file') throw new Error(sessionStoreMessage);
  const key = sessionKeyFromEnv(env);
  if (!env.NEUTRONSYNC_SESSION_FILE) throw new Error(sessionFileMessage);
  return new FileSessionStore(env.NEUTRONSYNC_SESSION_FILE, key, note);
}
