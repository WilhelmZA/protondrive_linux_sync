import { Fault } from './errors';
import legacySession from '../legacy-session.json';

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
}

export const keyringAttributes = ['application', 'neutronsync-drive', 'purpose', 'session-v1'];
export const keyringLabel = 'NeutronSync Drive session';
// Compatibility identifier only; never use it for a new session.
export const legacyAttributes = legacySession.attributes;

class InvalidStoredSession extends Fault {
  constructor() { super('fatal'); }
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
  async load(): Promise<Session | null> {
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
