// Exercises advisory RPC metadata through the real dispatcher and upload writer.
import type { Auth } from '../src/auth';
import type { Drive, ReadClient } from '../src/drive';
import type { UploadMetadata } from '@protontech/drive-sdk';
import { Writes, type WriteClient } from '../src/writes';
import { SafeLog } from '../src/log';
import { notifications, serve } from '../src/rpc';

const write = (value: unknown) => process.stdout.write(JSON.stringify(value) + '\n');
let metadata: UploadMetadata;
const client = {
  async *iterateFolderChildren() {},
  async getFileUploader(_: string, __: string, sent: UploadMetadata) {
    metadata = sent;
    return {
      async uploadFromStream(stream: ReadableStream, _: unknown, progress: (n: number) => void) {
        const bytes = await new Response(stream).arrayBuffer();
        progress(bytes.byteLength);
        return { completion: async () => {
          write({ jsonrpc: '2.0', method: 'test.metadata', params: metadata });
          return { nodeUid: 'file', nodeRevisionUid: 'revision' };
        } };
      },
    };
  },
} as unknown as ReadClient & WriteClient;
const writes = new Writes(client, notifications(write), new SafeLog(() => {}));
await serve({} as Auth, async () => ({ writes: () => writes }) as Drive, () => {}, write);
