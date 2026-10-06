// Explicit loopback-only transport seam for the compiled RPC contract test.
// Synthetic credentials and SRP proofs cannot reach Proton or Secret Service.
import type { Auth, AuthCrypto } from '../src/auth';
import type { ReadClient } from '../src/drive';
import type { Session, SessionStore } from '../src/store';
import type { NodeEntity } from '@protontech/drive-sdk';

export class MemoryStore implements SessionStore {
  value: Session | null = null;
  async load() { return this.value; }
  async save(value: Session) { this.value = { ...value }; }
  async clear() { this.value = null; }
}
export const fakeCrypto: AuthCrypto = {
  proof: async (_info, credentials) => ({ clientEphemeral: 'fake-ephemeral', clientProof: credentials.password === 'bad-password' ? 'bad-proof' : 'fake-proof', expectedServerProof: btoa('fake-server-proof'), sharedSession: new Uint8Array() }),
  keyPassword: async password => `salted-${password}`,
  unlock: async (_auth, password) => { if (password === 'salted-bad-password') throw new Error(); },
};
function hydrate(node: any): NodeEntity {
  if (node.activeRevision?.claimedModificationTime) node.activeRevision.claimedModificationTime = new Date(node.activeRevision.claimedModificationTime);
  if (node.folder?.claimedModificationTime) node.folder.claimedModificationTime = new Date(node.folder.claimedModificationTime);
  return node;
}
export function testDependencies(base: string) {
  const url = new URL(base);
  if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || url.username || url.password || url.pathname !== '/' || url.search || url.hash) throw new Error();
  const client = (auth: Auth): ReadClient => ({
    getMyFilesRootFolder: async () => hydrate(await auth.json('/fixture/root')),
    getNode: async uid => hydrate(await auth.json(`/fixture/node/${encodeURIComponent(uid)}`)),
    async *iterateFolderChildren(uid) { for (const node of await auth.json(`/fixture/children/${encodeURIComponent(uid)}`)) yield hydrate(node); },
    async *iterateEvents(scope, since) {
      const query = new URLSearchParams({ scope, ...(since ? { since } : {}) });
      for (const event of await auth.json(`/fixture/events?${query}`)) yield event;
    },
  });
  return { store: new MemoryStore(), crypto: fakeCrypto, client };
}
