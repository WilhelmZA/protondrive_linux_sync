/**
 * `GET /api/v1/events`.
 *
 * Yields the JSON object from each `event: state` dispatch. SSE comments
 * (`: keepalive`) and any other event name are skipped. Reconnect is off
 * unless `reconnect` is set.
 */

import { ApiError, type Client } from "./client.ts";
import type { Envelope } from "./openapi.ts";

const DEFAULT_BACKOFF_MS = [1000, 2000, 5000, 10000];

export type FollowEventsOptions = {
  /** Connect again after the stream ends or errors. Default false. */
  reconnect?: boolean;
  /** Delay before each reconnect, in milliseconds. The last value repeats. */
  backoffMs?: readonly number[];
};

export async function* followEvents(
  client: Client,
  options: FollowEventsOptions = {},
): AsyncGenerator<Envelope> {
  const reconnect = options.reconnect ?? false;
  const backoff = options.backoffMs?.length ? options.backoffMs : DEFAULT_BACKOFF_MS;
  let attempt = 0;
  for (;;) {
    try {
      for await (const envelope of readOnce(client)) {
        attempt = 0;
        yield envelope;
      }
    } catch (err) {
      if (!reconnect) throw err;
      await sleep(backoff[Math.min(attempt, backoff.length - 1)] ?? DEFAULT_BACKOFF_MS[0]);
      attempt += 1;
      continue;
    }
    if (!reconnect) return;
    await sleep(backoff[Math.min(attempt, backoff.length - 1)] ?? DEFAULT_BACKOFF_MS[0]);
    attempt += 1;
  }
}

async function* readOnce(client: Client): AsyncGenerator<Envelope> {
  const opened = await client.openEvents();
  try {
    const media = (opened.contentType.split(";")[0] ?? "").trim().toLowerCase();
    if (media !== "text/event-stream") {
      throw new ApiError(
        "bad_response",
        `Expected text/event-stream, got ${opened.contentType || "no content type"}`,
        opened.status,
      );
    }
    for await (const message of parseSse(opened.body)) {
      if (message.event !== "state") continue;
      yield parseEnvelope(message.data);
    }
  } finally {
    opened.body.destroy();
  }
}

type Pending = { event: string; data: string[] };

async function* parseSse(body: AsyncIterable<Uint8Array>): AsyncGenerator<{ event: string; data: string }> {
  const pending: Pending = { event: "", data: [] };
  let carry = "";
  for await (const chunk of body) {
    carry += Buffer.isBuffer(chunk) ? chunk.toString("utf8") : Buffer.from(chunk).toString("utf8");
    let newline = carry.indexOf("\n");
    while (newline !== -1) {
      let line = carry.slice(0, newline);
      carry = carry.slice(newline + 1);
      if (line.endsWith("\r")) line = line.slice(0, -1);
      const dispatched = pushLine(pending, line);
      if (dispatched) yield dispatched;
      newline = carry.indexOf("\n");
    }
  }
  if (carry.length > 0) {
    const dispatched = pushLine(pending, carry.endsWith("\r") ? carry.slice(0, -1) : carry);
    if (dispatched) yield dispatched;
  }
  const tail = take(pending);
  if (tail) yield tail;
}

/** One SSE line. Comments, including `: keepalive`, do not dispatch. */
function pushLine(pending: Pending, line: string): { event: string; data: string } | undefined {
  if (line.startsWith(":")) return undefined;
  if (line === "") return take(pending);
  const split = line.indexOf(":");
  const field = split === -1 ? line : line.slice(0, split);
  let value = split === -1 ? "" : line.slice(split + 1);
  if (value.startsWith(" ")) value = value.slice(1);
  if (field === "event") pending.event = value;
  else if (field === "data") pending.data.push(value);
  return undefined;
}

function take(pending: Pending): { event: string; data: string } | undefined {
  const data = pending.data.join("\n");
  const event = pending.event;
  pending.event = "";
  pending.data = [];
  if (!data) return undefined;
  return { event: event || "message", data };
}

function parseEnvelope(data: string): Envelope {
  let parsed: unknown;
  try {
    parsed = JSON.parse(data);
  } catch {
    throw new ApiError("bad_response", "State event was not JSON");
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new ApiError("bad_response", "State event was not an object");
  }
  return parsed as Envelope;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}
