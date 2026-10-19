import { expect, test } from 'bun:test';
import { createInterface } from 'node:readline';
import { spawn } from 'node:child_process';
import { mkdtemp, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { PassThrough } from 'node:stream';
import type { Auth } from '../src/auth';
import { AuthQueue, serve } from '../src/rpc';
import * as throttle from '../src/throttle';
import { BIG_SIZE, bigChunk } from './binary-backend';
import { binary } from './compiled';
import { fixture } from './fixture';

const KiB = 1024;

test('transfer.set_limits accepts two non-negative integers and rejects anything else as fatal', async () => {
  const input = new PassThrough();
  const replies: any[] = [];
  const served = serve({} as Auth, async () => { throw new Error('no drive needed'); }, () => {}, value => replies.push(value), input, new AuthQueue());
  const send = (id: number, params: unknown) => input.write(JSON.stringify({ jsonrpc: '2.0', id, method: 'transfer.set_limits', params }) + '\n');
  send(1, { upload_bytes_per_sec: 512 * KiB, download_bytes_per_sec: 0 });
  send(2, { upload_bytes_per_sec: 1 });
  send(3, { upload_bytes_per_sec: -1, download_bytes_per_sec: 0 });
  send(4, { upload_bytes_per_sec: 1.5, download_bytes_per_sec: 0 });
  send(5, { upload_bytes_per_sec: '1024', download_bytes_per_sec: 0 });
  send(6, { upload_bytes_per_sec: 0, download_bytes_per_sec: null });
  send(7, { upload_bytes_per_sec: 0, download_bytes_per_sec: 0 });
  input.end();
  await served;
  const reply = (id: number) => replies.find(r => r.id === id);
  expect(reply(1).result).toEqual({ ok: true });
  for (const id of [2, 3, 4, 5, 6]) expect(reply(id).error.data.code).toBe('fatal');
  expect(reply(7).result).toEqual({ ok: true });
  expect(throttle.upload.bytesPerSec).toBe(0);
  expect(throttle.download.bytesPerSec).toBe(0);
});

test('a 2 MiB download and upload take about 4 s at 512 KiB/s and no extra time at 0', async () => {
  const backend = fixture();
  const server = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch: backend.handle });
  const dir = await mkdtemp(join(tmpdir(), 'ns-limits-'));
  const child = spawn(binary, ['--test-backend', server.url.origin], { stdio: ['pipe', 'pipe', 'pipe'] });
  let next = 0;
  const pending = new Map<number, { resolve: (value: any) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> }>();
  const progress: { id: unknown; at: number }[] = [];
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => {
    const value = JSON.parse(line);
    if (value.method === 'transfer.progress') progress.push({ id: value.params.id, at: performance.now() });
    if (value.method) return;
    const waiter = pending.get(value.id);
    if (waiter) { pending.delete(value.id); clearTimeout(waiter.timer); waiter.resolve(value); }
  });
  const call = (method: string, params: object = {}) => new Promise<any>((resolve, reject) => {
    const id = ++next;
    const timer = setTimeout(() => reject(new Error(`RPC timeout: ${method}`)), 30_000);
    pending.set(id, { resolve, reject, timer });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  // Seconds taken, and the longest gap in seconds between the request and
  // its progress notifications: what the client's idle timeout sees.
  const timed = async (method: string, params: object) => {
    const started = performance.now();
    const id = next + 1;
    const reply = await call(method, params);
    expect(reply.error).toBeUndefined();
    const finished = performance.now();
    const marks = [started, ...progress.filter(p => p.id === id).map(p => p.at), finished];
    const quiet = Math.max(...marks.slice(1).map((t, i) => t - marks[i]!)) / 1000;
    return { seconds: (finished - started) / 1000, quiet, beats: progress.filter(p => p.id === id).length };
  };
  const source = join(dir, 'upload.bin');
  await writeFile(source, bigChunk(0, BIG_SIZE));
  const timings: Record<string, { seconds: number; quiet: number; beats: number }> = {};
  try {
    expect((await call('auth.login', { username: 'normal', password: 'password-secret' })).result).toEqual({ ok: true });
    expect((await call('transfer.set_limits', { upload_bytes_per_sec: 512 * KiB, download_bytes_per_sec: 512 * KiB })).result).toEqual({ ok: true });
    timings.limitedDownload = await timed('file.download', { uid: 'big', local_path: join(dir, 'limited.bin') });
    expect((await stat(join(dir, 'limited.bin'))).size).toBe(BIG_SIZE);
    timings.limitedUpload = await timed('file.upload', { parent_uid: 'documents', name: 'upload.bin', local_path: source });
    expect((await call('transfer.set_limits', { upload_bytes_per_sec: 0, download_bytes_per_sec: 0 })).result).toEqual({ ok: true });
    timings.freeDownload = await timed('file.download', { uid: 'big', local_path: join(dir, 'free.bin') });
    timings.freeUpload = await timed('file.upload', { parent_uid: 'documents', name: 'upload.bin', local_path: source });
    process.stdout.write(`speed-limit timings (s): ${JSON.stringify(timings)}\n`);
    expect(timings.limitedDownload!.seconds).toBeGreaterThanOrEqual(3.5);
    expect(timings.limitedDownload!.seconds).toBeLessThanOrEqual(5);
    expect(timings.limitedUpload!.seconds).toBeGreaterThanOrEqual(3.5);
    expect(timings.limitedUpload!.seconds).toBeLessThanOrEqual(5);
    expect(timings.freeDownload!.seconds).toBeLessThan(1.5);
    expect(timings.freeUpload!.seconds).toBeLessThan(1.5);
    // The fakes report progress once per block, like the SDK, so every
    // notification during the 4 s wait comes from the limiter's keepalive.
    for (const key of ['limitedDownload', 'limitedUpload'] as const) {
      expect(timings[key]!.beats).toBeGreaterThanOrEqual(3);
      expect(timings[key]!.quiet).toBeLessThanOrEqual(1.5);
    }
    child.stdin.end();
  } finally {
    for (const waiter of pending.values()) clearTimeout(waiter.timer);
    child.kill();
    lines.close();
    server.stop(true);
    await rm(dir, { recursive: true, force: true });
  }
}, 60_000);
