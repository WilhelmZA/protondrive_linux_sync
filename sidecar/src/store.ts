import { Fault } from './errors';

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

export const keyringAttributes = ['application', 'neutronsync-drive', 'purpose', 'phase0-session-v1'];
export const keyringLabel = 'NeutronSync Drive Phase 0 session';

export class SecretServiceStore implements SessionStore {
  private async run(args: string[], secret?: string) {
    try {
      const child = Bun.spawn(['secret-tool', ...args], {
        stdin: secret === undefined ? 'ignore' : new Blob([secret]), stdout: 'pipe', stderr: 'pipe',
      });
      const [output, , status] = await Promise.all([
        new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited,
      ]);
      return { output, status };
    } catch { throw new Fault('fatal'); }
  }
  async load(): Promise<Session | null> {
    const { output, status } = await this.run(['lookup', ...keyringAttributes]);
    if (status === 1 && !output) return null;
    if (status !== 0) throw new Fault('fatal');
    try {
      const value = JSON.parse(output);
      if (!['uid', 'accessToken', 'refreshToken', 'keyPassphrase', 'account'].every(k => typeof value[k] === 'string' && value[k]) || !Number.isFinite(value.expiresAt)) throw 0;
      return value;
    } catch { throw new Fault('fatal'); }
  }
  async save(session: Session) {
    const { status } = await this.run(['store', '--label', keyringLabel, ...keyringAttributes], JSON.stringify(session));
    if (status !== 0) throw new Fault('fatal');
  }
  async clear() {
    const { status } = await this.run(['clear', ...keyringAttributes]);
    if (status !== 0 && status !== 1) throw new Fault('fatal');
  }
}
