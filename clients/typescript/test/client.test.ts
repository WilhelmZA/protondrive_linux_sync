import assert from "node:assert/strict";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";

import { createClient, followEvents } from "../src/index.ts";

const ENVELOPE = {
  rev: 4,
  extra_top: true,
  state: {
    pairs: [],
    account: { checked: true, signed_in: true, account: "a" },
    activity: [],
    active: [],
    busy: false,
    watching: true,
    signed_out: false,
    future_field: 1,
  },
  held: [],
  conflicts: [],
  auto_sync: false,
};

type Seen = { method: string; path: string; headers: Record<string, string> };

function respond(socket: net.Socket, status: string, headers: Record<string, string>, body: string): void {
  const payload = Buffer.from(body, "utf8");
  const lines = [`HTTP/1.1 ${status}`];
  for (const [key, value] of Object.entries(headers)) lines.push(`${key}: ${value}`);
  lines.push(`Content-Length: ${payload.length}`, "Connection: close", "", "");
  socket.end(Buffer.concat([Buffer.from(`${lines.join("\r\n")}`), payload]));
}

function respondSse(socket: net.Socket, body: string): void {
  const head = [
    "HTTP/1.1 200 OK",
    "Content-Type: text/event-stream",
    "Cache-Control: no-store",
    "Connection: close",
    "",
    "",
  ].join("\r\n");
  socket.end(head + body);
}

function onConnection(socket: net.Socket, handle: (seen: Seen, socket: net.Socket) => void): void {
  let buf = Buffer.alloc(0);
  let done = false;
  socket.on("error", () => {});
  socket.on("data", (chunk) => {
    if (done) return;
    buf = Buffer.concat([buf, chunk]);
    const headerEnd = buf.indexOf("\r\n\r\n");
    if (headerEnd === -1) return;
    done = true;
    const head = buf.subarray(0, headerEnd).toString("utf8");
    const [start, ...headerLines] = head.split("\r\n");
    const [method = "", reqPath = ""] = (start ?? "").split(" ");
    const headers: Record<string, string> = {};
    for (const line of headerLines) {
      const split = line.indexOf(":");
      if (split === -1) continue;
      headers[line.slice(0, split).toLowerCase()] = line.slice(split + 1).trim();
    }
    handle({ method, path: reqPath, headers }, socket);
  });
}

async function listenSocket(handle: (seen: Seen, socket: net.Socket) => void): Promise<{
  socketPath: string;
  close: () => Promise<void>;
}> {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "neutronsync-ts-"));
  const socketPath = path.join(dir, "api.sock");
  const server = net.createServer((socket) => onConnection(socket, handle));
  await new Promise<void>((resolve) => server.listen(socketPath, resolve));
  return {
    socketPath,
    close: () =>
      new Promise((resolve) => {
        server.close(() => {
          fs.rmSync(dir, { recursive: true, force: true });
          resolve();
        });
      }),
  };
}

async function listenTcp(handle: (seen: Seen, socket: net.Socket) => void): Promise<{
  baseUrl: string;
  close: () => Promise<void>;
}> {
  const server = net.createServer((socket) => onConnection(socket, handle));
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("tcp listen failed");
  return {
    baseUrl: `http://127.0.0.1:${address.port}`,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

test("state() over a unix socket ignores unknown fields", async () => {
  const server = await listenSocket((seen, socket) => {
    assert.equal(seen.method, "GET");
    assert.equal(seen.path, "/api/v1/state?activity=100");
    assert.equal(seen.headers.authorization, "Bearer secret");
    assert.equal(seen.headers.host, "x");
    assert.equal(seen.headers.origin, undefined);
    assert.equal(seen.headers["sec-fetch-site"], undefined);
    respond(socket, "200 OK", { "Content-Type": "application/json" }, JSON.stringify(ENVELOPE));
  });
  const client = createClient({
    baseUrl: "http://x",
    socketPath: server.socketPath,
    token: "secret",
  });
  try {
    const envelope = await client.state();
    assert.equal(envelope.rev, 4);
    assert.equal(envelope.state.watching, true);
    assert.equal(envelope.state.account.account, "a");
    assert.equal(client.socketPath, server.socketPath);
  } finally {
    await server.close();
  }
});

test("followEvents can stop after the first envelope", async () => {
  const payload = `event: state\ndata: ${JSON.stringify(ENVELOPE)}\n\n: keepalive\n\n`;
  const server = await listenSocket((_seen, socket) => {
    respondSse(socket, payload);
  });
  const client = createClient({ baseUrl: "http://localhost", socketPath: server.socketPath });
  try {
    let seen = 0;
    for await (const envelope of followEvents(client)) {
      assert.equal(envelope.rev, 4);
      seen += 1;
      break;
    }
    assert.equal(seen, 1);
  } finally {
    await server.close();
  }
});

test("followEvents yields state and skips keepalive", async () => {
  const payload = `event: state\ndata: ${JSON.stringify(ENVELOPE)}\n\n: keepalive\n\nevent: other\ndata: hi\n\n`;
  const server = await listenSocket((seen, socket) => {
    assert.equal(seen.path, "/api/v1/events");
    assert.ok(seen.headers.accept?.includes("text/event-stream"));
    assert.equal(seen.headers.origin, undefined);
    assert.equal(seen.headers["sec-fetch-site"], undefined);
    respondSse(socket, payload);
  });
  const client = createClient({ baseUrl: "http://localhost", socketPath: server.socketPath });
  try {
    const revs: number[] = [];
    for await (const envelope of followEvents(client)) revs.push(envelope.rev);
    assert.deepEqual(revs, [4]);
  } finally {
    await server.close();
  }
});

test("omitting socketPath uses TCP", async () => {
  const server = await listenTcp((seen, socket) => {
    assert.equal(seen.method, "GET");
    assert.equal(seen.path, "/api/v1/state?activity=100");
    assert.equal(seen.headers.authorization, undefined);
    assert.equal(seen.headers.origin, undefined);
    respond(socket, "200 OK", { "Content-Type": "application/json" }, JSON.stringify(ENVELOPE));
  });
  const client = createClient({ baseUrl: server.baseUrl });
  try {
    assert.equal(client.socketPath, undefined);
    const envelope = await client.state();
    assert.equal(envelope.rev, 4);
  } finally {
    await server.close();
  }
});
