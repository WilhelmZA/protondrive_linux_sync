import { expect, test } from 'bun:test';
import { createInterface } from 'node:readline';
import { spawn } from 'node:child_process';
import { resolve } from 'node:path';
import { fixture } from './fixture';

test('compiled binary implements the complete RPC contract against fake HTTP', async () => {
  const backend = fixture();
  const server = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch: backend.handle });
  const child = spawn(resolve(import.meta.dir, '../dist/neutronsync-drive'), ['--test-backend', server.url.origin], { stdio: ['pipe', 'pipe', 'pipe'] });
  const notifications: any[] = [];
  const protocol: any[] = [];
  let stderr = '';
  let next = 0;
  const pending = new Map<number | null, { resolve: (value: any) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> }>();
  const exited = new Promise<number | null>(resolve => child.on('exit', resolve));
  child.stderr.on('data', data => { stderr += data; });
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => {
    try {
      const value = JSON.parse(line);
      protocol.push(value);
      expect(value.jsonrpc).toBe('2.0');
      if (value.method) notifications.push(value);
      else {
        const waiter = pending.get(value.id);
        if (waiter) { pending.delete(value.id); clearTimeout(waiter.timer); waiter.resolve(value); }
      }
    } catch {
      for (const waiter of pending.values()) waiter.reject(new Error('Non-protocol stdout'));
    }
  });
  const call = (method: string, params: object = {}, malformed = false) => new Promise<any>((resolve, reject) => {
    const id = malformed ? null : ++next;
    const timer = setTimeout(() => reject(new Error('RPC timeout')), 15_000);
    pending.set(id, { resolve, reject, timer });
    child.stdin.write(malformed ? '{bad json\n' : JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  try {
    expect((await call('', {}, true)).error).toMatchObject({ code: -32700, data: { code: 'fatal' } });
    expect((await call('auth.status')).result).toEqual({ signed_in: false, account: null });
    expect((await call('auth.login', { username: 'totp-two-password', password: 'password-secret' })).result).toEqual({ need_2fa: true });
    expect((await call('auth.submit_2fa', { code: '123456' })).result).toEqual({ need_mailbox_password: true });
    expect((await call('auth.submit_mailbox_password', { password: 'mailbox-secret' })).result).toEqual({ ok: true });
    expect((await call('auth.status')).result.signed_in).toBe(true);
    expect((await call('node.resolve', { path: '/my-files/Documents' })).result).toEqual({ uid: 'documents', type: 'folder' });
    const list = await call('node.list', { uid: 'documents' });
    expect(list.result.entries).toHaveLength(2);
    expect(list.result.entries[1]).toMatchObject({ mtime: null, sha1: null });
    expect((await call('node.walk', { uid: 'root', exclude_globs: ['**/unknown.txt'] })).result).toEqual({ folders: 2, failed: [] });
    expect(notifications.filter(n => n.method === 'walk.entry').map(n => n.params.uid)).toEqual(['documents', 'file']);
    expect((await call('events.subscribe', { scope_id: '/my-files' })).result).toEqual({ ok: true, last_event_id: 'event-0' });
    expect(notifications.some(n => n.method === 'events.batch' && n.params.scope_id === 'sdk-scope-id')).toBe(true);
    expect((await call('events.subscribe', { scope_id: 'replay-scope', since_event_id: 'saved-cursor' })).result.ok).toBe(true);
    const replay = notifications.find(n => n.method === 'events.batch' && n.params.scope_id === 'replay-scope');
    expect(replay.params.events.map((e: any) => e.type)).toEqual(['node_created', 'node_updated', 'node_deleted']);
    expect(replay.params.events[0]).toMatchObject({ node_uid: 'file', parent_uid: 'documents' });
    expect(new Set(notifications.filter(n => n.method === 'events.refresh_required').map(n => n.params.reason))).toEqual(new Set(['fast_forward', 'tree_refresh', 'tree_remove']));
    for (const [uid, code] of Object.entries({ missing: 'not_found', denied: 'auth', conflict: 'conflict', transient: 'transient', fatal: 'fatal', limited: 'rate_limited' })) {
      const response = await call('node.list', { uid });
      expect(response.result).toBeUndefined();
      expect(response.error.data.code).toBe(code);
      expect(typeof response.error.code).toBe('number');
      if (code === 'rate_limited') expect(response.error.data.retry_after).toBe(7);
    }
    expect((await call('node.list', { uid: 'expired' })).error.data.code).toBe('auth');
    expect(notifications.some(n => n.method === 'auth.signed_out')).toBe(true);
    expect((await call('auth.status')).result.signed_in).toBe(false);
    expect((await call('auth.logout')).result).toEqual({ ok: true });
    expect((await call('unknown.method')).error.data.code).toBe('fatal');
    expect(notifications.some(n => n.method === 'log')).toBe(true);
    const output = JSON.stringify(protocol) + stderr;
    for (const secret of ['password-secret', 'mailbox-secret', '123456', 'access-secret', 'refresh-secret', 'salted-mailbox-secret']) expect(output).not.toContain(secret);
    expect(backend.requests.every(r => r.version === 'external-drive-neutronsync@0.1.0-dev')).toBe(true);
    child.stdin.end();
    expect(await exited).toBe(0);
  } finally {
    for (const waiter of pending.values()) clearTimeout(waiter.timer);
    child.kill();
    lines.close();
    server.stop(true);
  }
}, 30_000);
