// Bandwidth limits. One bucket per direction, shared by every connection and
// every transfer in this process, so one limit caps the whole machine's
// NeutronSync traffic. Rate 0 means no limit: no wait, no timer.
import { Transform } from 'node:stream';

export type Clock = () => number;
export type Sleep = (ms: number, signal?: AbortSignal) => Promise<void>;

const MIN_CAPACITY = 64 * 1024;
// A bucket nobody has drawn from for this long is idle: the next transfer
// starts with an empty bucket instead of a free second of burst.
const IDLE_MS = 1000;
// While the limiter holds a transfer back, its keepalive runs this often, so
// a deliberate wait (even one queued behind other transfers, or many short
// ones) keeps the client's idle timeout alive. It stops as soon as the
// limiter is out of the picture, so a stalled network still times out.
export const HEARTBEAT_MS = 1000;

// One per transfer. Beats once per HEARTBEAT_MS while the transfer has been
// held by a bucket during that interval; silent otherwise. Starts its timer
// on the first hold, never at rate 0, and stops at `end()`.
export class Keepalive {
  private held = 0;
  private marked = false;
  private stop?: AbortController;
  constructor(private beat: () => void) {}
  hold(sleep: Sleep) {
    this.held++;
    this.marked = true;
    if (!this.stop) {
      this.stop = new AbortController();
      void this.loop(sleep, this.stop.signal);
    }
  }
  release() { this.held--; }
  end() { this.stop?.abort(); }
  private async loop(sleep: Sleep, signal: AbortSignal) {
    try {
      for (;;) {
        await sleep(HEARTBEAT_MS, signal);
        if (this.held > 0 || this.marked) {
          this.marked = false;
          this.beat();
        }
      }
    } catch { /* ended */ }
  }
}

export function realSleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(signal.reason); return; }
    const onAbort = () => { clearTimeout(timer); reject(signal!.reason); };
    const timer = setTimeout(() => { signal?.removeEventListener('abort', onAbort); resolve(); }, ms);
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

export class TokenBucket {
  private rate = 0;
  private capacity = MIN_CAPACITY;
  private tokens = 0;
  private last: number;
  private inflight = 0;
  private queue: Promise<unknown> = Promise.resolve();
  constructor(private now: Clock = () => performance.now(), private sleep: Sleep = realSleep) {
    this.last = now();
  }

  get bytesPerSec() { return this.rate; }

  // Test hook: drive the shared bucket from a fake clock.
  setClock(now: Clock, sleep: Sleep) {
    this.now = now;
    this.sleep = sleep;
    this.last = now();
  }

  // Replaces the rate. The bucket starts empty, so the first second of a
  // transfer is throttled like every other second.
  setRate(bytesPerSec: number) {
    this.rate = bytesPerSec;
    this.capacity = Math.max(bytesPerSec, MIN_CAPACITY);
    this.tokens = 0;
    this.last = this.now();
  }

  // Resolves once `bytes` may be sent. Waiters are served in order, so two
  // concurrent transfers share one rate instead of one starving the other.
  // The transfer's `keepalive` is held for the whole wait, queue included.
  take(bytes: number, signal?: AbortSignal, keepalive?: Keepalive): Promise<void> {
    if (this.rate === 0 || bytes <= 0) return Promise.resolve();
    if (signal?.aborted) return Promise.reject(signal.reason);
    if (this.inflight === 0 && this.now() - this.last >= IDLE_MS) {
      this.tokens = 0;
      this.last = this.now();
    }
    this.inflight++;
    keepalive?.hold(this.sleep);
    const turn = this.queue.then(() => this.grant(bytes, signal));
    this.queue = turn.catch(() => {}).finally(() => { this.inflight--; keepalive?.release(); });
    if (!signal) return turn;
    // A queued waiter rejects at once on abort, not when its turn comes.
    return new Promise<void>((resolve, reject) => {
      const onAbort = () => reject(signal.reason);
      signal.addEventListener('abort', onAbort, { once: true });
      turn.then(resolve, reject).finally(() => signal.removeEventListener('abort', onAbort));
    });
  }

  private refill() {
    const now = this.now();
    const elapsed = Math.max(0, now - this.last) / 1000;
    this.last = now;
    this.tokens = Math.min(this.capacity, this.tokens + elapsed * this.rate);
  }

  private async grant(bytes: number, signal?: AbortSignal) {
    let remaining = bytes;
    while (remaining > 0) {
      if (signal?.aborted) throw signal.reason;
      if (this.rate === 0) return;
      this.refill();
      // A chunk larger than the capacity takes tokens in pieces.
      const piece = Math.min(remaining, this.capacity);
      if (this.tokens >= piece) {
        this.tokens -= piece;
        remaining -= piece;
        continue;
      }
      await this.sleep(((piece - this.tokens) / this.rate) * 1000, signal);
    }
  }
}

// A stream stage that lets bytes through at the bucket's rate, counting the
// bytes it has passed so far.
export function throttled(bucket: TokenBucket, keepalive?: Keepalive): Transform & { passed: number } {
  const stage = new Transform({
    transform(chunk: Buffer, _encoding, callback) {
      bucket.take(chunk.length, undefined, keepalive)
        .then(() => { stage.passed += chunk.length; callback(null, chunk); }, callback);
    },
  }) as Transform & { passed: number };
  stage.passed = 0;
  return stage;
}

export const upload = new TokenBucket();
export const download = new TokenBucket();

export function setLimits(uploadBytesPerSec: number, downloadBytesPerSec: number) {
  upload.setRate(uploadBytesPerSec);
  download.setRate(downloadBytesPerSec);
  return { ok: true };
}
