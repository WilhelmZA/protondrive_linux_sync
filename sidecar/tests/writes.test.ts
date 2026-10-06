import { expect, test } from 'bun:test';
import { createHash } from 'node:crypto';
import { mkdtemp, writeFile, readFile, readdir, rm, stat, utimes } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { NodeEntity, UploadMetadata } from '@protontech/drive-sdk';
import { Writes, type WriteClient } from '../src/writes';
import { entry, type ReadClient } from '../src/drive';
import { SafeLog } from '../src/log';
import { Fault } from '../src/errors';
import { Auth, appVersion } from '../src/auth';
import { httpClient } from '../src/http';
import { MemoryStore, fakeCrypto } from './binary-backend';
import { fixture } from './fixture';

const content = Buffer.from('verified upload bytes');
const sha1 = createHash('sha1').update(content).digest('hex');
function setup() {
  const nodes: NodeEntity[] = [];
  const calls: any[] = [];
  const notes: any[] = [];
  const logs: string[] = [];
  let revision: any = { claimedSize: content.length, claimedDigests: { sha1: sha1.toUpperCase() }, claimedModificationTime: new Date(1700000000999) };
  let signature = false;
  let fail = false;
  let abort: (() => void) | undefined;
  const node = (name: string, type = 'file') => ({ uid: name, name: { ok: true, value: name }, type, activeRevision: revision }) as NodeEntity;
  const uploader = (kind: string, metadata: UploadMetadata) => {
    calls.push({ kind, metadata });
    return {
      async uploadFromStream(stream: ReadableStream, _: unknown, progress: (bytes: number) => void) {
        const bytes = Buffer.from(await new Response(stream).arrayBuffer());
        expect(bytes).toEqual(content);
        progress(bytes.length);
        return { completion: async () => ({ nodeUid: 'file', nodeRevisionUid: 'revision' }) };
      },
    };
  };
  const client = {
    async *iterateFolderChildren() { yield* nodes; },
    getNode: async () => node('file'),
    async createFolder(_: string, name: string) { const n = node(name, 'folder'); nodes.push(n); calls.push('mkdir'); return n; },
    getFileUploader: async (_: string, __: string, metadata: UploadMetadata) => uploader('new', metadata),
    getFileRevisionUploader: async (_: string, metadata: UploadMetadata) => uploader('revision', metadata),
    getFileDownloader: async () => ({
      downloadToStream(stream: WritableStream, progress: (n: number) => void) {
        return {
          async completion() {
            const writer = stream.getWriter();
            try {
              await writer.write(content);
              progress(content.length);
              abort?.();
              await writer.close();
            } finally { writer.releaseLock(); }
            if (fail || signature) throw new Fault('fatal');
          },
          isDownloadCompleteWithSignatureIssues: () => signature,
        };
      },
    }),
    async renameNode(uid: string, name: string) { calls.push({ uid, name }); },
    async *moveNodes(uids: string[], parent: string) { calls.push({ uids, parent }); yield { uid: uids[0]!, ok: true }; },
    async *trashNodes(uids: string[]) { for (const uid of uids) yield uid === 'bad' ? { uid, ok: false, error: new Fault('auth') } : { uid, ok: true }; },
  } as unknown as ReadClient & WriteClient;
  return {
    writes: new Writes(client, (method, params) => notes.push({ method, params }), new SafeLog(line => logs.push(line))),
    nodes, node, calls, notes, logs,
    revision: (value: any) => { revision = value; },
    signature: () => { signature = true; }, failure: () => { fail = true; }, abort: (fn: () => void) => { abort = fn; },
  };
}

test('folder creation is idempotent, rejects file collisions, and reports batch failures', async () => {
  const s = setup();
  expect(await s.writes.createFolder('root', 'folder')).toEqual({ uid: 'folder' });
  expect(await s.writes.createFolder('root', 'folder')).toEqual({ uid: 'folder' });
  expect(s.calls).toEqual(['mkdir']);
  s.nodes.push(s.node('file'));
  await expect(s.writes.createFolder('root', 'file')).rejects.toMatchObject({ code: 'conflict' });
  await expect(s.writes.createFolder('root', '../escape')).rejects.toMatchObject({ code: 'fatal' });
  expect(await s.writes.rename('file', 'renamed')).toEqual({ ok: true });
  expect(await s.writes.move('file', 'folder')).toEqual({ ok: true });
  await expect(s.writes.trash(['file', 'bad'])).rejects.toMatchObject({ code: 'auth' });
});

test('upload uses exact snapshot metadata and replaces by name or explicit uid', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'ns-write-test-'));
  try {
    const path = join(dir, 'file.txt');
    await writeFile(path, content);
    await utimes(path, 1700000000.999, 1700000000.999);
    const s = setup();
    const result = await s.writes.upload('root', 'file.txt', path, 7);
    expect(result).toEqual({ uid: 'file', revision_uid: 'revision', sha1, size: content.length, mtime: 1700000000 });
    expect(s.calls[0]).toEqual({ kind: 'new', metadata: { mediaType: 'text/plain;charset=utf-8', expectedSize: content.length, expectedSha1: sha1, modificationTime: new Date(1700000000000) } });
    s.nodes.push(s.node('file.txt'));
    await s.writes.upload('root', 'file.txt', path, '8');
    expect(s.calls[1].kind).toBe('revision');
    await s.writes.upload('root', 'other.txt', path, 9, 'explicit');
    expect(s.calls[2].kind).toBe('revision');
    s.nodes.push(s.node('folder', 'folder'));
    await expect(s.writes.upload('root', 'folder', path, 10)).rejects.toMatchObject({ code: 'conflict' });
    expect(s.notes[0]).toEqual({ method: 'transfer.progress', params: { id: 7, bytes: content.length, total: content.length } });
    expect(entry(s.node('file')).sha1).toBe(sha1);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('download verifies claims, atomically publishes, and retains whole-second mtime', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'ns-download-test-'));
  try {
    const s = setup();
    const path = join(dir, 'file');
    expect(await s.writes.download('file', path, 42)).toEqual({ sha1, size: content.length, claimed_size: content.length, claimed_sha1: sha1 });
    expect(await readFile(path)).toEqual(content);
    expect((await stat(path)).mtimeMs).toBe(1700000000000);
    expect(await readdir(dir)).toEqual(['file']);
    expect(s.notes).toEqual([{ method: 'transfer.progress', params: { id: 42, bytes: content.length, total: content.length } }]);
    s.revision({});
    expect(await s.writes.download('file', path, 43)).toMatchObject({ claimed_size: null, claimed_sha1: null });
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('RPC upload ignores disagreeing advisory size and mtime', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'ns-upload-rpc-'));
  try {
    const path = join(dir, 'file.txt');
    await writeFile(path, content);
    await utimes(path, 1700000000.999, 1700000000.999);
    const child = Bun.spawn([process.execPath, join(import.meta.dir, 'upload-rpc-backend.ts')], { stdin: 'pipe', stdout: 'pipe', stderr: 'pipe' });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 71, method: 'file.upload', params: { parent_uid: 'root', name: 'file.txt', local_path: path, size: 999999, mtime: 1 } }) + '\n');
    child.stdin.end();
    const notes = (await new Response(child.stdout).text()).trim().split('\n').map(line => JSON.parse(line));
    expect(await child.exited).toBe(0);
    expect(notes.find(n => n.method === 'test.metadata').params).toMatchObject({ expectedSize: content.length, expectedSha1: sha1, modificationTime: '2023-11-14T22:13:20.000Z' });
    expect(notes.find(n => n.id === 71).result).toMatchObject({ sha1, size: content.length, mtime: 1700000000 });
  } finally { await rm(dir, { recursive: true, force: true }); }
});

for (const scenario of ['sha1', 'size', 'exception', 'abort', 'signature-no-claim', 'signature-bad-claim', 'signature-matched'] as const) {
  test(`download cleanup: ${scenario}`, async () => {
    const dir = await mkdtemp(join(tmpdir(), 'ns-failed-download-'));
    try {
      const s = setup();
      const controller = new AbortController();
      if (scenario === 'sha1' || scenario === 'signature-bad-claim') s.revision({ claimedDigests: { sha1: 'bad' } });
      if (scenario === 'size') s.revision({ claimedSize: content.length + 1 });
      if (scenario === 'exception') s.failure();
      if (scenario === 'abort') s.abort(() => controller.abort());
      if (scenario.startsWith('signature')) s.signature();
      if (scenario === 'signature-no-claim') s.revision({});
      const run = s.writes.download('file', join(dir, 'file'), 1, controller.signal);
      if (scenario === 'signature-matched') {
        await run;
        expect(await readdir(dir)).toEqual(['file']);
        expect(s.logs[0]).toContain('Download signature issue accepted after digest verification');
      } else {
        await expect(run).rejects.toThrow();
        expect(await readdir(dir)).toEqual([]);
      }
    } finally { await rm(dir, { recursive: true, force: true }); }
  });
}

test('HTTP allowlist rejects before fetching and blobs isolate session headers with passthrough', async () => {
  const backend = fixture();
  const seen: { url: string; init: RequestInit }[] = [];
  let response = new Response('rate limited', { status: 429 });
  const auth = new Auth(new MemoryStore(), fakeCrypto, () => {}, new SafeLog(() => {}), async (url, init) => {
    if (!url.includes('/drive/') && !url.includes('storage.example')) return backend.fetch(url, init);
    seen.push({ url, init });
    return response;
  });
  await auth.login('normal', 'password-secret');
  const http = httpClient(auth);
  const request = { url: 'https://drive-api.proton.me/drive/v2/volumes/v/trash', method: 'DELETE', headers: new Headers(), timeoutMs: 5000 };
  for (const method of ['DELETE', 'PATCH', 'POST', 'PUT']) await expect(http.fetchJson({ ...request, method })).rejects.toMatchObject({ code: 'fatal' });
  expect(seen).toHaveLength(0);
  for (const [method, path] of [
    ['POST', 'drive/v2/volumes/v/links'], ['POST', 'drive/v2/volumes/v/folders'],
    ['POST', 'drive/v2/volumes/v/trash_multiple'], ['POST', 'drive/v2/volumes/v/links/n/checkAvailableHashes'],
    ['POST', 'drive/v2/volumes/v/files'], ['POST', 'drive/v2/volumes/v/files/small'],
    ['POST', 'drive/v2/volumes/v/files/n/revisions'], ['POST', 'drive/v2/volumes/v/files/n/revisions/small'],
    ['POST', 'drive/blocks'], ['PUT', 'drive/v2/volumes/v/links/n/rename'],
    ['PUT', 'drive/v2/volumes/v/links/n/move'], ['PUT', 'drive/v2/volumes/v/files/n/revisions/r'],
  ]) {
    expect(await http.fetchJson({ ...request, method: method!, url: `https://drive-api.proton.me/${path}`, json: { test: true } })).toBe(response);
  }
  const body = new FormData(); body.set('Metadata', 'test');
  await http.fetchJson({ ...request, method: 'POST', url: 'https://drive-api.proton.me/drive/v2/volumes/v/files/small', body });
  expect(new Headers(seen.at(-1)!.init.headers).has('Content-Type')).toBe(false);
  expect(seen.at(-1)!.init.body).toBe(body);
  const headers = new Headers({ Authorization: 'do-not-leak', 'x-pm-uid': 'do-not-leak', 'pm-storage-token': 'storage' });
  const controller = new AbortController();
  expect(await http.fetchBlob({ ...request, method: 'GET', url: 'https://storage.example/block', headers, signal: controller.signal })).toBe(response);
  const sent = new Headers(seen.at(-1)!.init.headers);
  expect(sent.has('Authorization')).toBe(false);
  expect(sent.has('x-pm-uid')).toBe(false);
  expect(sent.get('x-pm-appversion')).toBe(appVersion);
  expect(sent.get('pm-storage-token')).toBe('storage');
  controller.abort();
  expect(seen.at(-1)!.init.signal!.aborted).toBe(true);
  response = new Response('server error', { status: 503 });
  expect(await http.fetchBlob({ ...request, method: 'POST', headers, body })).toBe(response);
  const sameOrigin = new Headers(seen.at(-1)!.init.headers);
  expect(sameOrigin.get('Authorization')).toStartWith('Bearer ');
  expect(sameOrigin.has('x-pm-uid')).toBe(true);
});
