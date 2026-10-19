import { expect, test } from 'bun:test';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { NodeEntity } from '@protontech/drive-sdk';
import { HEARTBEAT_MS, Keepalive, TokenBucket, realSleep } from '../src/throttle';
import * as throttle from '../src/throttle';
import { Writes, type WriteClient } from '../src/writes';
import type { ReadClient } from '../src/drive';
import { SafeLog } from '../src/log';

const KiB = 1024;
const MiB = 1024 * KiB;

// A clock whose time moves only when a timer fires. Every sleep registers a
// timer; `run` fires them in order until the consumers settle.
function fakeClock() {
  let now = 0;
  let timers: { at: number; resolve: () => void; reject: (e: unknown) => void; signal?: AbortSignal }[] = [];
  let sleeps = 0;
  const sleep = (ms: number, signal?: AbortSignal) => new Promise<void>((resolve, reject) => {
    sleeps++;
    const timer = { at: now + ms, resolve, reject, signal };
    timers.push(timer);
    signal?.addEventListener('abort', () => { timers = timers.filter(t => t !== timer); reject(signal.reason); }, { once: true });
  });
  const settle = () => new Promise<void>(resolve => setImmediate(resolve));
  const run = async (done: () => boolean) => {
    for (let i = 0; i < 100_000; i++) {
      await settle();
      if (done()) return;
      if (!timers.length) {
        // Real async work (file i/o) may still be on its way to the next sleep.
        const waited = Date.now();
        while (!timers.length && !done() && Date.now() - waited < 2000) await new Promise(resolve => setTimeout(resolve, 5));
        if (done()) return;
        if (!timers.length) throw new Error('stalled: no timer pending');
      }
      timers.sort((a, b) => a.at - b.at);
      const next = timers.shift()!;
      now = Math.max(now, next.at);
      next.resolve();
    }
    throw new Error('did not finish');
  };
  return { now: () => now, sleep, run, get sleeps() { return sleeps; }, advance(ms: number) { now += ms; } };
}

async function push(bucket: TokenBucket, bytes: number, chunk = 64 * KiB, signal?: AbortSignal, keepalive?: Keepalive) {
  for (let sent = 0; sent < bytes; sent += chunk) await bucket.take(Math.min(chunk, bytes - sent), signal, keepalive);
}

test('1 MiB at 100 KiB/s takes about 10 s of fake time', async () => {
  const clock = fakeClock();
  const bucket = new TokenBucket(clock.now, clock.sleep);
  bucket.setRate(100 * KiB);
  let finished = false;
  const work = push(bucket, MiB).then(() => { finished = true; });
  await clock.run(() => finished);
  await work;
  expect(clock.now()).toBeGreaterThanOrEqual(9_000);
  expect(clock.now()).toBeLessThanOrEqual(11_000);
});

test('rate 0 adds no wait and creates no timer', async () => {
  const clock = fakeClock();
  const bucket = new TokenBucket(clock.now, clock.sleep);
  await push(bucket, 10 * MiB);
  bucket.setRate(100 * KiB);
  bucket.setRate(0);
  await push(bucket, 10 * MiB);
  expect(clock.sleeps).toBe(0);
  expect(clock.now()).toBe(0);
});

test('two concurrent consumers share one rate', async () => {
  const clock = fakeClock();
  const bucket = new TokenBucket(clock.now, clock.sleep);
  const rate = 100 * KiB;
  bucket.setRate(rate);
  let done = 0;
  const a = push(bucket, 512 * KiB, 32 * KiB).then(() => { done++; });
  const b = push(bucket, 512 * KiB, 48 * KiB).then(() => { done++; });
  await clock.run(() => done === 2);
  await Promise.all([a, b]);
  const throughput = MiB / (clock.now() / 1000);
  expect(Math.abs(throughput - rate) / rate).toBeLessThanOrEqual(0.1);
});

test('a chunk larger than the capacity is taken in pieces', async () => {
  const clock = fakeClock();
  const bucket = new TokenBucket(clock.now, clock.sleep);
  bucket.setRate(64 * KiB);
  let finished = false;
  const work = bucket.take(4 * MiB).then(() => { finished = true; });
  await clock.run(() => finished);
  await work;
  expect(clock.now()).toBeGreaterThanOrEqual(63_000);
  expect(clock.now()).toBeLessThanOrEqual(65_000);
});

test('an aborted wait rejects promptly, also while queued', async () => {
  const clock = fakeClock();
  const bucket = new TokenBucket(clock.now, clock.sleep);
  bucket.setRate(1 * KiB);
  const head = new AbortController();
  const queued = new AbortController();
  const first = bucket.take(64 * KiB, head.signal);
  const second = bucket.take(64 * KiB, queued.signal);
  let firstSettled = false;
  let secondSettled = false;
  first.catch(() => {}).finally(() => { firstSettled = true; });
  second.catch(() => {}).finally(() => { secondSettled = true; });
  await new Promise(resolve => setImmediate(resolve));
  queued.abort(new Error('cancelled'));
  await expect(second).rejects.toThrow('cancelled');
  await new Promise(resolve => setImmediate(resolve));
  expect(secondSettled).toBe(true);
  expect(firstSettled).toBe(false);
  expect(clock.now()).toBe(0);
  head.abort(new Error('cancelled'));
  await expect(first).rejects.toThrow('cancelled');
  // The bucket is still usable afterwards.
  bucket.setRate(0);
  await bucket.take(MiB);
});

test('an idle bucket starts the next transfer empty instead of with a free burst', async () => {
  const clock = fakeClock();
  const bucket = new TokenBucket(clock.now, clock.sleep);
  bucket.setRate(64 * KiB);
  let finished = false;
  const first = bucket.take(64 * KiB).then(() => { finished = true; });
  await clock.run(() => finished);
  await first;
  expect(clock.now()).toBeGreaterThanOrEqual(1_000);
  const after = clock.now();
  clock.advance(5_000);
  finished = false;
  const second = bucket.take(64 * KiB).then(() => { finished = true; });
  await clock.run(() => finished);
  await second;
  expect(clock.now() - after - 5_000).toBeGreaterThanOrEqual(1_000);
});

test('a held transfer beats every second, queued or in many short waits, and never at rate 0', async () => {
  const clock = fakeClock();
  const bucket = new TokenBucket(clock.now, clock.sleep);
  const beats: { who: string; at: number }[] = [];
  const keepalive = (who: string) => new Keepalive(() => beats.push({ who, at: clock.now() }));
  // Rate 0: no hold, no timer, no beat.
  const idle = keepalive('idle');
  await bucket.take(MiB, undefined, idle);
  expect(clock.sleeps).toBe(0);
  bucket.setRate(KiB);
  let done = 0;
  // The head waits 64 s for one piece; the queued transfer beats meanwhile.
  const head = keepalive('head');
  const queued = keepalive('queued');
  const first = bucket.take(64 * KiB, undefined, head).then(() => { done++; });
  const second = bucket.take(KiB, undefined, queued).then(() => { done++; });
  await clock.run(() => done === 2);
  await Promise.all([first, second]);
  head.end(); queued.end();
  const gaps = (who: string) => {
    const at = beats.filter(b => b.who === who).map(b => b.at);
    return at.slice(1).map((t, i) => t - at[i]!);
  };
  expect(beats.filter(b => b.who === 'queued').length).toBeGreaterThanOrEqual(60);
  expect(Math.max(...gaps('queued'), ...gaps('head'))).toBeLessThanOrEqual(HEARTBEAT_MS);
  expect(beats.filter(b => b.who === 'idle')).toHaveLength(0);
  // Many short waits (64 KiB pieces at 512 KiB/s, 125 ms each) still beat once a second.
  bucket.setRate(512 * KiB);
  const short = keepalive('short');
  done = 0;
  const stream = push(bucket, 2 * MiB, 64 * KiB, undefined, short).then(() => { done++; });
  await clock.run(() => done === 1);
  await stream;
  const shortBeats = beats.filter(b => b.who === 'short');
  expect(shortBeats.length).toBeGreaterThanOrEqual(3);
  expect(Math.max(...gaps('short'))).toBeLessThanOrEqual(HEARTBEAT_MS);
  // Nothing beats once the transfer is over.
  short.end();
  const after = beats.length;
  clock.advance(10_000);
  await new Promise(resolve => setImmediate(resolve));
  expect(beats.length).toBe(after);
});

// R1-001: the SDK reports network progress before it flushes a block into the
// throttled sink, so a slow limit used to leave the client's idle timeout
// without any progress for the whole wait. With the keepalive, a 2 MiB block
// at 1 KiB/s (2048 s) never goes more than a second without a notification.
test('a slow download keeps reporting progress while the limiter holds the block', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'ns-slow-download-'));
  const clock = fakeClock();
  const bucket = throttle.download;
  bucket.setClock(clock.now, clock.sleep);
  try {
    bucket.setRate(KiB);
    const notes: { params: any; at: number }[] = [];
    const total = 2 * MiB;
    const body = new Uint8Array(total);
    const node = { uid: 'big', name: { ok: true, value: 'big' }, type: 'file', activeRevision: { claimedSize: total } } as unknown as NodeEntity;
    const client = {
      getNode: async () => node,
      getFileDownloader: async () => ({
        downloadToStream(stream: WritableStream<Uint8Array>, progress: (n: number) => void) {
          return {
            async completion() {
              const writer = stream.getWriter();
              try { progress(total); await writer.write(body); await writer.close(); } finally { writer.releaseLock(); }
            },
            isDownloadCompleteWithSignatureIssues: () => false,
          };
        },
      }),
    } as unknown as ReadClient & WriteClient;
    const writes = new Writes(client, (method, params) => { if (method === 'transfer.progress') notes.push({ params, at: clock.now() }); }, new SafeLog(() => {}));
    let finished = false;
    const run = writes.download('big', join(dir, 'big'), 7).then(() => { finished = true; });
    await clock.run(() => finished);
    await run;
    expect((await stat(join(dir, 'big'))).size).toBe(total);
    // The limiter alone accounts for 2048 s; fake time also moves on heartbeat
    // ticks while the real file writes finish, so there is no upper bound here.
    expect(clock.now()).toBeGreaterThanOrEqual(2_048_000);
    expect(notes.every(n => n.params.id === 7)).toBe(true);
    const at = notes.map(n => n.at);
    const gaps = at.slice(1).map((t, i) => t - at[i]!);
    expect(notes.length).toBeGreaterThanOrEqual(2000);
    expect(Math.max(...gaps)).toBeLessThanOrEqual(HEARTBEAT_MS);
    // The limiter releases the block at 2048 s; the keepalive beats at most
    // once more and is silent from then on, so a stalled network would still
    // time out.
    const last = at[at.length - 1]!;
    expect(last).toBeGreaterThanOrEqual(2_047_000);
    expect(last).toBeLessThanOrEqual(2_048_000 + HEARTBEAT_MS);
  } finally {
    bucket.setRate(0);
    bucket.setClock(() => performance.now(), realSleep);
    await rm(dir, { recursive: true, force: true });
  }
});
