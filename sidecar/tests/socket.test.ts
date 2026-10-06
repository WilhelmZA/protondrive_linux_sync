import { expect, test } from 'bun:test';
import { connect, type Socket } from 'node:net';
import { createInterface } from 'node:readline';
import { mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { binary } from './compiled';
import { fixture } from './fixture';

async function client(path: string) {
  let socket: Socket | undefined;
  for (let i = 0; i < 100; i++) {
    socket = await new Promise<Socket | undefined>(resolve => {
      const s = connect(path);
      s.once('connect', () => resolve(s));
      s.once('error', () => { s.destroy(); resolve(undefined); });
    });
    if (socket) break;
    await Bun.sleep(50);
  }
  if (!socket) throw new Error('socket did not start');
  const stream = socket;
  const notes: any[] = [];
  const pending = new Map<number, (value: any) => void>();
  let id = 0;
  const lines = createInterface({ input: stream });
  lines.on('line', line => {
    const value = JSON.parse(line);
    if (value.method) notes.push(value);
    else { pending.get(value.id)?.(value); pending.delete(value.id); }
  });
  return {
    notes,
    close() { stream.destroy(); lines.close(); },
    call(method: string, params = {}): Promise<any> {
      const key = ++id;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { pending.delete(key); reject(new Error('RPC timeout')); }, 10000);
        pending.set(key, value => { clearTimeout(timer); resolve(value); });
        stream.write(JSON.stringify({ jsonrpc: '2.0', id: key, method, params }) + '\n');
      });
    },
  };
}

test('two concurrent clients share one owner with isolated walks, feed batches and ack cursors', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'ns-socket-'));
  const path = join(directory, 'session.sock');
  const backend = fixture();
  const polls: (string | null)[] = [];
  const server = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === '/fixture/events') {
      const since = url.searchParams.get('since');
      polls.push(since);
      return Response.json(since === 'start' ? [{ type: 'node_created', nodeUid: 'file', parentNodeUid: 'documents', eventId: 'new' }] : []);
    }
    return backend.handle(request);
  } });
  const start = () => Bun.spawn([binary, '--socket', path, '--test-backend', server.url.origin], { stdout: 'ignore', stderr: 'pipe' });
  const first = start(), contender = start();
  const a = await client(path), b = await client(path);
  try {
    expect(statSync(directory).mode & 0o777).toBe(0o700);
    expect((await a.call('auth.login', { username: 'normal', password: 'password-secret' })).result.ok).toBe(true);
    expect((await b.call('auth.status')).result.signed_in).toBe(true);
    const owner = a.notes.find(n => n.method === 'sidecar.hello').params.pid;
    expect(b.notes.find(n => n.method === 'sidecar.hello').params.pid).toBe(owner);
    const loser = owner === first.pid ? contender : first;
    expect(await loser.exited).toBe(0);
    await Promise.all([
      a.call('node.walk', { uid: 'root', exclude_globs: ['**/unknown.txt'] }),
      b.call('node.walk', { uid: 'documents', exclude_globs: ['sample.txt'] }),
    ]);
    expect(a.notes.filter(n => n.method === 'walk.entry').map(n => n.params.uid)).toEqual(['documents', 'file']);
    expect(b.notes.filter(n => n.method === 'walk.entry').map(n => n.params.uid)).toEqual(['no-metadata']);
    await a.call('events.subscribe', { scope_id: 'scope', since_event_id: 'start', ack_required: true });
    expect(b.notes.filter(n => n.method === 'events.batch')).toHaveLength(0);
    await b.call('events.subscribe', { scope_id: 'scope', since_event_id: 'start', ack_required: true });
    expect(polls).toEqual(['start', 'start']);
    await a.call('events.ack', { scope_id: 'scope', event_id: 'new' });
    await Bun.sleep(5200);
    expect(polls).toEqual(['start', 'start', 'new']);
    expect(b.notes.filter(n => n.method === 'events.batch').every(n => n.params.events.length === 1)).toBe(true);
    await b.call('events.ack', { scope_id: 'scope', event_id: 'new' });
    await a.call('auth.logout');
    await Bun.sleep(20);
    expect(a.notes.some(n => n.method === 'auth.signed_out')).toBe(true);
    expect(b.notes.some(n => n.method === 'auth.signed_out')).toBe(true);
    (owner === first.pid ? first : contender).kill('SIGKILL');
    await Promise.all([first.exited, contender.exited]);
    // A stale socket and two simultaneous replacement owners still elect one.
    const next = start(), other = start();
    const c = await client(path);
    try {
      await c.call('auth.status');
      const pid = c.notes.find(n => n.method === 'sidecar.hello').params.pid;
      expect(await (pid === next.pid ? other : next).exited).toBe(0);
    } finally { c.close(); next.kill(); other.kill(); await Promise.all([next.exited, other.exited]); }
  } finally {
    a.close(); b.close(); first.kill(); contender.kill();
    await Promise.all([first.exited, contender.exited]);
    server.stop(true); rmSync(directory, { recursive: true, force: true });
  }
}, 30000);
