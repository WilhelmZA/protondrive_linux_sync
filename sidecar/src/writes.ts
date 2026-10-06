import { createHash, randomUUID } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { copyFile, mkdtemp, open, rename, rm, stat, utimes } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { Readable } from 'node:stream';
import type { ProtonDriveClient, NodeResult } from '@protontech/drive-sdk';
import type { ReadClient } from './drive';
import { Fault, classify } from './errors';
import type { Notify, SafeLog } from './log';

export type WriteClient = Pick<ProtonDriveClient, 'createFolder' | 'renameNode' | 'moveNodes' | 'trashNodes' | 'getFileUploader' | 'getFileRevisionUploader' | 'getFileDownloader'>;
export function safeName(name: string) {
  if (!name || name === '.' || name === '..' || /[\\/\0]/.test(name)) throw new Fault('fatal');
}

export class Writes {
  constructor(private client: ReadClient & WriteClient, private notify: Notify, private log: SafeLog) {}

  private async child(parent: string, name: string) {
    safeName(name);
    let match;
    for await (const node of this.client.iterateFolderChildren(parent)) {
      if (!node.name.ok) throw new Fault('fatal');
      if (node.name.value !== name) continue;
      if (match) throw new Fault('conflict');
      match = node;
    }
    return match;
  }

  async createFolder(parent: string, name: string): Promise<{ uid: string }> {
    const existing = await this.child(parent, name);
    if (existing) {
      if (existing.type !== 'folder') throw new Fault('conflict');
      return { uid: existing.uid };
    }
    try { return { uid: (await this.client.createFolder(parent, name)).uid }; }
    catch (error) {
      if (classify(error).code !== 'conflict') throw error;
      const raced = await this.child(parent, name);
      if (raced?.type === 'folder') return { uid: raced.uid };
      throw error;
    }
  }

  async upload(parent: string, name: string, path: string, id: unknown, replace?: string) {
    safeName(name);
    const existing = replace ? undefined : await this.child(parent, name);
    if (existing && existing.type !== 'file') throw new Fault('conflict');
    const snapshot = await mkdtemp(join(tmpdir(), 'neutronsync-upload-'));
    const source = join(snapshot, 'content');
    try {
      const before = await stat(path);
      if (!before.isFile()) throw new Fault('fatal');
      // An immutable snapshot makes metadata describe exactly the uploaded bytes.
      await copyFile(path, source);
      const after = await stat(path);
      if (before.size !== after.size || before.mtimeMs !== after.mtimeMs) throw new Fault('transient');
      const size = (await stat(source)).size;
      const hash = createHash('sha1');
      for await (const chunk of createReadStream(source)) hash.update(chunk);
      const sha1 = hash.digest('hex');
      const mtime = Math.floor(before.mtimeMs / 1000);
      const metadata = { mediaType: Bun.file(path).type || 'application/octet-stream', expectedSize: size, expectedSha1: sha1, modificationTime: new Date(mtime * 1000) };
      const uid = replace ?? existing?.uid;
      const uploader = uid ? await this.client.getFileRevisionUploader(uid, metadata) : await this.client.getFileUploader(parent, name, metadata);
      const input = createReadStream(source);
      try {
        const controller = await uploader.uploadFromStream(Readable.toWeb(input) as unknown as ReadableStream, [], bytes => this.notify('transfer.progress', { id, bytes, total: size }));
        const result = await controller.completion();
        return { uid: result.nodeUid, revision_uid: result.nodeRevisionUid, sha1, size, mtime };
      } finally { input.destroy(); }
    } finally { await rm(snapshot, { recursive: true, force: true }); }
  }

  async download(uid: string, path: string, id: unknown, signal?: AbortSignal) {
    const node = await this.client.getNode(uid);
    if (node.type !== 'file') throw new Fault('conflict');
    const revision = node.activeRevision;
    const claimedSize = revision?.claimedSize ?? null;
    const claimedSha1 = revision?.claimedDigests?.sha1?.toLowerCase() || null;
    const temp = join(dirname(path), `.neutronsync-${randomUUID()}.tmp`);
    const file = await open(temp, 'wx', 0o600);
    try {
      const hash = createHash('sha1');
      let size = 0;
      const stream = new WritableStream<Uint8Array>({
        async write(chunk) {
          signal?.throwIfAborted();
          let offset = 0;
          while (offset < chunk.length) {
            const { bytesWritten } = await file.write(chunk, offset, chunk.length - offset);
            if (!bytesWritten) throw new Fault('fatal');
            offset += bytesWritten;
          }
          hash.update(chunk); size += chunk.length;
        },
      });
      const downloader = await this.client.getFileDownloader(uid, signal);
      const controller = downloader.downloadToStream(stream, bytes => this.notify('transfer.progress', { id, bytes, total: claimedSize }));
      try { await controller.completion(); }
      catch (error) { if (!controller.isDownloadCompleteWithSignatureIssues()) throw error; }
      signal?.throwIfAborted();
      const sha1 = hash.digest('hex');
      if ((claimedSize !== null && size !== claimedSize) || (claimedSha1 !== null && sha1 !== claimedSha1)) throw new Fault('fatal');
      if (controller.isDownloadCompleteWithSignatureIssues()) {
        if (!claimedSha1) throw new Fault('fatal');
        const msg = 'Download signature issue accepted after digest verification';
        this.log.write('warn', msg);
        this.notify('log', { level: 'warn', msg });
      }
      await file.sync();
      await file.close();
      if (revision?.claimedModificationTime) {
        const seconds = Math.floor(revision.claimedModificationTime.getTime() / 1000);
        await utimes(temp, seconds, seconds);
      }
      await rename(temp, path);
      return { sha1, size, claimed_sha1: claimedSha1, claimed_size: claimedSize };
    } finally {
      try { await file.close(); }
      finally { await rm(temp, { force: true }); }
    }
  }

  async rename(uid: string, name: string) {
    safeName(name);
    await this.client.renameNode(uid, name);
    return { ok: true };
  }
  private async results(results: AsyncIterable<NodeResult>, uids: string[]) {
    const pending = new Set(uids);
    let failure: unknown;
    for await (const result of results) {
      pending.delete(result.uid);
      if (!result.ok) failure ??= result.error;
    }
    if (failure) throw failure;
    if (pending.size) throw new Fault('fatal');
    return { ok: true };
  }
  move(uid: string, parent: string) { return this.results(this.client.moveNodes([uid], parent), [uid]); }
  trash(uids: string[]) { return this.results(this.client.trashNodes(uids), uids); }
}
