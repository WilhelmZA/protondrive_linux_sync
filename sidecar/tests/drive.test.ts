import { expect, test } from 'bun:test';
import type { NodeEntity } from '@protontech/drive-sdk';
import { Drive, entry, type ReadClient } from '../src/drive';
import { Auth } from '../src/auth';
import { SafeLog } from '../src/log';
import { MemoryStore, fakeCrypto, testDependencies } from './binary-backend';
import { fixture } from './fixture';

test('missing folder and auth failure are errors, never empty listings', async () => {
  const backend = fixture();
  const log = new SafeLog(() => {});
  const auth = new Auth(new MemoryStore(), fakeCrypto, () => {}, log, backend.fetch);
  await auth.login('normal', 'password-secret');
  const drive = new Drive(testDependencies('http://127.0.0.1:1').client(auth), () => {}, log);
  await expect(drive.list('missing')).rejects.toMatchObject({ code: 'not_found' });
  await expect(drive.list('denied')).rejects.toMatchObject({ code: 'auth' });
  expect((await drive.list('documents')).entries).toHaveLength(2);
});

test('claimed metadata preserves nulls and epoch seconds', () => {
  const backend = fixture();
  const file = { ...backend.file, activeRevision: { ...backend.file.activeRevision, claimedModificationTime: new Date('2026-01-02T03:04:05Z') } };
  expect(entry(file as unknown as NodeEntity)).toMatchObject({ name: 'sample.txt', size: 42, mtime: 1767323045, sha1: 'abc123', uid: 'file', parent_uid: 'documents' });
  expect(entry(backend.missingMetadata as unknown as NodeEntity)).toMatchObject({ mtime: null, sha1: null });
});

test('partial child iteration cannot return a successful partial list', async () => {
  const backend = fixture();
  const client: ReadClient = {
    getNode: async () => backend.folder as NodeEntity,
    getMyFilesRootFolder: async () => backend.root as NodeEntity,
    async *iterateFolderChildren() { yield backend.missingMetadata as NodeEntity; throw new Error('page failure'); },
    async *iterateEvents() {},
  };
  const drive = new Drive(client, () => {}, new SafeLog(() => {}));
  await expect(drive.list('documents')).rejects.toThrow();
});

test('walk excludes globs, streams entries and records failed subfolders', async () => {
  const backend = fixture();
  const nodes = [backend.folder, { ...backend.folder, uid: 'broken', name: { ok: true, value: 'broken' } }];
  const notifications: any[] = [];
  const client: ReadClient = {
    getNode: async () => backend.root as NodeEntity,
    getMyFilesRootFolder: async () => backend.root as NodeEntity,
    async *iterateFolderChildren(uid) {
      if (uid === 'broken') throw new Error();
      for (const node of uid === 'root' ? nodes : [backend.missingMetadata]) yield node as NodeEntity;
    },
    async *iterateEvents() {},
  };
  const drive = new Drive(client, (_method, params) => notifications.push(params), new SafeLog(() => {}));
  expect(await drive.walk('root', ['**/*.txt'])).toEqual({ folders: 2, failed: ['broken'], failed_codes: ['fatal'] });
  expect(notifications.map(n => n.entry.name)).toEqual(['Documents', 'broken']);
});
