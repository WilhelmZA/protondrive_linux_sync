/**
 * Read engine state, take one event, and start one sync.
 *
 * Point this at a neutronsync serve you already started. It does not launch the server.
 *
 *   node examples/sync-once.ts --socket "$XDG_RUNTIME_DIR/neutronsync/api.sock"
 *   node examples/sync-once.ts --base-url http://127.0.0.1:8090 --token "$NEUTRONSYNC_API_TOKEN"
 *
 * A program that depends on this package imports `@neutronsync/client`.
 * This file imports the source so it runs from the repo.
 */

import { createClient, followEvents } from "../src/index.ts";

function arg(name: string): string | undefined {
  const index = process.argv.indexOf(name);
  if (index === -1) return undefined;
  return process.argv[index + 1];
}

const socketPath = arg("--socket") ?? process.env.NEUTRONSYNC_SOCKET;
const token = arg("--token") ?? process.env.NEUTRONSYNC_API_TOKEN;
const baseFlag = arg("--base-url") ?? process.env.NEUTRONSYNC_API_URL;
const dryRun = process.argv.includes("--dry-run");
const baseUrl = baseFlag ?? (socketPath ? "http://localhost" : "http://127.0.0.1:8090");

const client = createClient({ baseUrl, token, socketPath });
const state = await client.state();
console.log(`rev ${state.rev} pairs ${state.state.pairs.length} busy ${state.state.busy}`);
for await (const envelope of followEvents(client)) {
  console.log(`event rev ${envelope.rev} busy ${envelope.state.busy}`);
  break;
}
await client.sync(dryRun ? { dry_run: true } : {});
console.log(dryRun ? "dry run posted" : "sync started");
