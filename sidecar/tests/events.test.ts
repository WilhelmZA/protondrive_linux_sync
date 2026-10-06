import { expect, test } from 'bun:test';
import type { DriveEvent, NodeEntity } from '@protontech/drive-sdk';
import { Drive, type ReadClient } from '../src/drive';
import { SafeLog } from '../src/log';

function harness() {
  const notes: { method: string; params: any }[] = [];
  const polls: (string | undefined)[] = [];
  const nodes = new Map<string, any>([
    ['root', { uid: 'root', type: 'folder', name: { ok: true, value: 'root' }, treeEventScopeId: 'scope' }],
    ['folder', { uid: 'folder', type: 'folder', parentUid: 'root', name: { ok: true, value: 'folder' } }],
  ]);
  const client: ReadClient = {
    async getMyFilesRootFolder() { return nodes.get('root') as NodeEntity; },
    async getNode(uid) { if (!nodes.has(uid)) throw { code: 'not_found' }; return nodes.get(uid); },
    async *iterateFolderChildren() {},
    async *iterateEvents(_scope, since) {
      polls.push(since);
      if (!since) yield { type: 'fast_forward', eventId: 'start' } as DriveEvent;
      else if (since === 'start') yield { type: 'node_created', eventId: 'new', nodeUid: 'child', parentNodeUid: 'folder' } as DriveEvent;
    },
  };
  const drive = new Drive(client, (method, params) => notes.push({ method, params }), new SafeLog(() => {}), 10);
  return { drive, notes, polls, nodes };
}

test('first subscribe drains history without emitting batches; acknowledged delivery replays until ack', async () => {
  const h = harness();
  try {
    expect(await h.drive.subscribe('/my-files', undefined, true)).toMatchObject({ scope_id: 'scope', last_event_id: 'start' });
    expect(h.notes).toHaveLength(0);
    await Bun.sleep(35);
    expect(h.notes.length).toBeGreaterThanOrEqual(2);
    expect(h.notes[0]!.params).toEqual(h.notes[1]!.params);
    h.drive.ack('scope', 'start');
    h.drive.ack('scope', 'new');
    await Bun.sleep(25);
    expect(h.polls).toContain('new');
    expect(() => h.drive.ack('scope', 'never-delivered')).toThrow();
    expect(() => h.drive.ack('unknown', 'new')).toThrow();
  } finally { h.drive.close(); }
});

test('resumed subscribe starts after supplied cursor and default delivery advances automatically', async () => {
  const h = harness();
  try {
    await h.drive.subscribe('scope', 'start');
    expect(h.polls[0]).toBe('start');
    expect(h.notes[0]!.params.events[0].node_uid).toBe('child');
    await Bun.sleep(25);
    expect(h.polls[1]).toBe('new');
  } finally { h.drive.close(); }
});

test('node.path resolves parent links and rejects trash, unknown nodes and cycles', async () => {
  const h = harness();
  expect(await h.drive.path('folder')).toEqual({ path: '/my-files/folder' });
  await expect(h.drive.path('missing')).rejects.toMatchObject({ code: 'not_found' });
  h.nodes.get('folder').trashTime = new Date();
  await expect(h.drive.path('folder')).rejects.toMatchObject({ code: 'not_found' });
  delete h.nodes.get('folder').trashTime;
  h.nodes.get('folder').parentUid = 'folder';
  await expect(h.drive.path('folder')).rejects.toMatchObject({ code: 'not_found' });
});
