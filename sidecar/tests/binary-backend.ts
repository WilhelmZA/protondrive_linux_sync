// Explicit loopback-only transport seam for the compiled RPC contract test.
// Synthetic credentials and SRP proofs cannot reach Proton or Secret Service.
import type { Auth, AuthCrypto } from '../src/auth';
import type { ReadClient } from '../src/drive';
import type { Session, SessionStore } from '../src/store';
import type { WriteClient } from '../src/writes';
import type { NodeEntity } from '@protontech/drive-sdk';

// The 2 MiB body behind the fixture's `big` node, used by the speed-limit test.
export const BIG_SIZE = 2 * 1024 * 1024;
export function bigChunk(offset: number, length: number): Uint8Array {
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) out[i] = (offset + i) % 251;
  return out;
}

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
  // Transfers never leave the process: an upload is read and discarded, a
  // download is generated. Both flow through the sidecar's real stream stages
  // in the SDK's order: an upload reads a whole block before it reports
  // progress, a download reports network progress before it flushes the block
  // into the sink. So while the limiter holds a block back, the fake reports
  // nothing, exactly like the SDK.
  const uploader = () => ({
    async uploadFromStream(stream: ReadableStream<Uint8Array>, _: unknown, progress: (bytes: number) => void) {
      let received = 0;
      for await (const chunk of stream as unknown as AsyncIterable<Uint8Array>) received += chunk.length;
      progress(received);
      return { completion: async () => ({ nodeUid: 'uploaded', nodeRevisionUid: 'revision' }) };
    },
  });
  const client = (auth: Auth): ReadClient & WriteClient => ({
    getMyFilesRootFolder: async () => hydrate(await auth.json('/fixture/root')),
    getNode: async (uid: string) => hydrate(await auth.json(`/fixture/node/${encodeURIComponent(uid)}`)),
    async *iterateFolderChildren(uid: string) { for (const node of await auth.json(`/fixture/children/${encodeURIComponent(uid)}`)) yield hydrate(node); },
    async *iterateEvents(scope: string, since?: string) {
      const query = new URLSearchParams({ scope, ...(since ? { since } : {}) });
      for (const event of await auth.json(`/fixture/events?${query}`)) yield event;
    },
    getFileUploader: async () => uploader(),
    getFileRevisionUploader: async () => uploader(),
    getFileDownloader: async (uid: string) => ({
      downloadToStream(stream: WritableStream<Uint8Array>, progress: (bytes: number) => void) {
        return {
          async completion() {
            const writer = stream.getWriter();
            try {
              const total = uid === 'big' ? BIG_SIZE : 0;
              progress(total);
              if (total) await writer.write(bigChunk(0, total));
              await writer.close();
            } finally { writer.releaseLock(); }
          },
          isDownloadCompleteWithSignatureIssues: () => false,
        };
      },
    }),
  } as unknown as ReadClient & WriteClient);
  return { store: new MemoryStore(), crypto: fakeCrypto, client };
}
