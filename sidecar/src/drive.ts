import { CryptoProxy } from '@protontech/crypto';
import { getSrp, computeKeyPassword, generateKeySalt } from '@protontech/crypto/srp';
import {
  ProtonDriveClient, MemoryCache, OpenPGPCryptoWithCryptoProxy,
  type NodeEntity, type DriveEvent, type ProtonDriveHTTPClientJsonRequest,
} from '@protontech/drive-sdk';
import { Account } from './account';
import type { Auth } from './auth';
import { Fault, classify } from './errors';
import type { Notify, SafeLog } from './log';

export interface ReadClient {
  getMyFilesRootFolder(): Promise<NodeEntity>;
  getNode(uid: string): Promise<NodeEntity>;
  iterateFolderChildren(uid: string): AsyncIterable<NodeEntity>;
  iterateEvents(scope: string, since?: string): AsyncIterable<DriveEvent>;
}

export function sdkClient(auth: Auth, account: Account, log: SafeLog): ReadClient {
  const fetchJson = async (request: ProtonDriveHTTPClientJsonRequest) => {
    const method = request.method.toUpperCase();
    // The SDK batches metadata reads through this POST endpoint.
    const metadataRead = method === 'POST' && /^\/drive\/v2\/volumes\/[^/]+\/links$/.test(new URL(request.url).pathname);
    if (method !== 'GET' && !metadataRead) throw new Fault('fatal');
    const timeout = AbortSignal.timeout(request.timeoutMs || 30_000);
    return auth.request(request.url, {
      method, headers: request.headers, body: metadataRead ? JSON.stringify(request.json) : undefined,
      signal: request.signal ? AbortSignal.any([request.signal, timeout]) : timeout,
    });
  };
  const diagnostic = () => log.write('debug', 'SDK diagnostic suppressed');
  const sdk = new ProtonDriveClient({
    httpClient: { fetchJson, fetchBlob: async () => { throw new Fault('fatal'); } },
    account, entitiesCache: new MemoryCache(), cryptoCache: new MemoryCache(),
    openPGPCryptoModule: new OpenPGPCryptoWithCryptoProxy(CryptoProxy),
    srpModule: {
      getSrp: (Version, Modulus, ServerEphemeral, Salt, password) => getSrp({ Version, Modulus, ServerEphemeral, Salt, Username: '' }, { username: '', password }),
      computeKeyPassword, generateKeySalt,
      getSrpVerifier: async () => { throw new Fault('fatal'); },
    },
    telemetry: { getLogger: () => ({ debug: diagnostic, info: diagnostic, warn: diagnostic, error: diagnostic }), recordMetric: () => {} },
  });
  return {
    getMyFilesRootFolder: () => sdk.getMyFilesRootFolder(),
    async getNode(uid) {
      for await (const node of sdk.iterateNodes([uid])) {
        if ('missingUid' in node) throw new Fault('not_found');
        return node;
      }
      throw new Fault('not_found');
    },
    async *iterateFolderChildren(uid) {
      let batch: string[] = [];
      const read = async function* (uids: string[]) {
        let count = 0;
        for await (const node of sdk.iterateNodes(uids)) {
          if ('missingUid' in node) throw new Fault('not_found');
          count++;
          yield node;
        }
        if (count !== uids.length) throw new Fault('not_found');
      };
      for await (const child of sdk.iterateFolderChildrenNodeUids(uid)) {
        batch.push(child);
        if (batch.length === 100) { yield* read(batch); batch = []; }
      }
      if (batch.length) yield* read(batch);
    },
    iterateEvents: (scope, since) => sdk.iterateEvents(scope, since),
  };
}

export interface Entry {
  name: string;
  type: 'file' | 'folder';
  size: number;
  mtime: number | null;
  sha1: string | null;
  uid: string;
  parent_uid: string | null;
}

export function entry(node: NodeEntity): Entry {
  if (!node.name.ok || node.errors?.length || !['file', 'folder'].includes(node.type)) throw new Fault('fatal');
  const name = node.name.value;
  if (!name || name === '.' || name === '..' || /[\/\0]/.test(name)) throw new Fault('fatal');
  const revision = node.activeRevision;
  const mtime = revision?.claimedModificationTime ?? node.folder?.claimedModificationTime;
  return {
    name, type: node.type as Entry['type'], size: revision?.claimedSize ?? 0,
    mtime: mtime ? Math.floor(mtime.getTime() / 1000) : null,
    sha1: revision?.claimedDigests?.sha1 || null, uid: node.uid, parent_uid: node.parentUid ?? null,
  };
}

export class Drive {
  private scopes = new Map<string, { cursor?: string; timer?: ReturnType<typeof setTimeout>; active: boolean; polling?: Promise<void> }>();
  constructor(private client: ReadClient, private notify: Notify, private log: SafeLog, private pollMs = 5000) {}
  async resolve(path: string) {
    const parts = path.split('/').filter(Boolean);
    if (parts.shift() !== 'my-files' || parts.some(p => p === '.' || p === '..')) throw new Fault('not_found');
    let node = await this.client.getMyFilesRootFolder();
    for (const part of parts) {
      const children = await this.list(node.uid);
      const matches = children.entries.filter(e => e.name === part);
      if (!matches.length) throw new Fault('not_found');
      if (matches.length > 1) throw new Fault('conflict');
      node = await this.client.getNode(matches[0]!.uid);
    }
    return { uid: node.uid, type: node.type };
  }
  async list(uid: string) {
    // Fetch the parent first: an SDK iterator can otherwise return nothing for a missing parent.
    const parent = await this.client.getNode(uid);
    if (parent.type !== 'folder') throw new Fault('conflict');
    const entries: Entry[] = [];
    for await (const node of this.client.iterateFolderChildren(uid)) entries.push(entry(node));
    return { entries };
  }
  async walk(uid: string, excludeGlobs: string[]) {
    const globs = excludeGlobs.map(pattern => new Bun.Glob(pattern));
    const queue = [{ uid, path: '' }];
    const seen = new Set<string>();
    const failed: string[] = [];
    let folders = 0;
    while (queue.length) {
      const chunk = queue.splice(0, 8);
      const results = await Promise.allSettled(chunk.map(async folder => {
        if (seen.has(folder.uid)) { failed.push(folder.path); return; }
        seen.add(folder.uid);
        try {
          const { entries } = await this.list(folder.uid);
          folders++;
          for (const child of entries) {
            const path = folder.path ? `${folder.path}/${child.name}` : child.name;
            if (globs.some(g => g.match(path) || (child.type === 'folder' && g.match(`${path}/`)))) continue;
            this.notify('walk.entry', { uid: child.uid, parent_uid: folder.uid, entry: child });
            if (child.type === 'folder') queue.push({ uid: child.uid, path });
          }
        } catch (error) {
          const fault = classify(error);
          if (fault.code === 'auth' || folder.uid === uid) throw fault;
          failed.push(folder.path);
          this.log.write('warn', 'Folder listing failed');
        }
      }));
      const rejected = results.find(result => result.status === 'rejected');
      if (rejected?.status === 'rejected') throw rejected.reason;
    }
    return { folders, failed };
  }
  async subscribe(scope: string, since?: string) {
    // The path alias allows the spike to discover the SDK scope without adding an RPC method.
    if (scope === '/my-files') scope = (await this.client.getMyFilesRootFolder()).treeEventScopeId;
    const existing = this.scopes.get(scope);
    if (existing) return { ok: true, last_event_id: existing.cursor ?? null };
    const state = { cursor: since, active: true } as { cursor?: string; active: boolean; timer?: ReturnType<typeof setTimeout>; polling?: Promise<void> };
    this.scopes.set(scope, state);
    const poll = async () => {
      const events: { type: string; node_uid: string; parent_uid: string | null }[] = [];
      let cursor = state.cursor;
      for await (const event of this.client.iterateEvents(scope, state.cursor)) {
        if (!state.active) return;
        cursor = event.eventId;
        if (['tree_refresh', 'tree_remove', 'fast_forward'].includes(event.type)) {
          this.notify('events.refresh_required', { scope_id: scope, reason: event.type });
        } else if ('nodeUid' in event) {
          events.push({ type: event.type, node_uid: event.nodeUid, parent_uid: event.parentNodeUid ?? null });
        }
      }
      if (!state.active) return;
      state.cursor = cursor;
      this.notify('events.batch', { scope_id: scope, events, last_event_id: cursor ?? null });
    };
    const tick = async () => {
      if (!state.active) return;
      try { state.polling = poll(); await state.polling; }
      catch (error) {
        this.log.write('warn', 'Event polling failed');
        const fault = classify(error);
        if (fault.code === 'auth') { state.active = false; return; }
        if (fault.code === 'rate_limited') {
          state.timer = setTimeout(tick, Math.max(this.pollMs, (fault.retry_after ?? 60) * 1000));
          return;
        }
      }
      if (state.active) state.timer = setTimeout(tick, this.pollMs);
    };
    try { state.polling = poll(); await state.polling; }
    catch (error) { state.active = false; this.scopes.delete(scope); throw error; }
    if (state.active) state.timer = setTimeout(tick, this.pollMs);
    return { ok: true, last_event_id: state.cursor ?? null };
  }
  close() {
    for (const state of this.scopes.values()) { state.active = false; clearTimeout(state.timer); }
    this.scopes.clear();
  }
}
