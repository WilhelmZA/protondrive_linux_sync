import { createInterface } from 'node:readline';
import type { Readable } from 'node:stream';
import { hvTypes, type Auth, type HvType } from './auth';
import type { Drive } from './drive';
import { Fault, classify } from './errors';
import type { Notify } from './log';

const codes = { not_found: -32004, auth: -32001, conflict: -32009, rate_limited: -32029, transient: -32050, fatal: -32603 };
let connectionNumber = 0;
const traceFields: Record<string, string[]> = {
  'node.resolve': ['path'], 'node.path': ['uid'], 'node.list': ['uid'],
  'node.walk': ['uid', 'exclude_globs'], 'node.create_folder': ['parent_uid', 'name'],
  'file.upload': ['parent_uid', 'name', 'local_path', 'replace_uid'],
  'file.download': ['uid', 'local_path'], 'node.rename': ['uid', 'new_name'],
  'node.move': ['uid', 'new_parent_uid'], 'node.trash': ['uids'],
  'events.subscribe': ['scope_id', 'since_event_id', 'ack_required'],
  'events.ack': ['scope_id', 'event_id'],
};
// Opt-in diagnostics retain routing and filesystem operations, never auth
// parameters or auth results (including human-verification tokens).
export function rpcTrace(connection: number, direction: string, value: unknown) {
  if (process.env.NEUTRONSYNC_DRIVE_TRACE === '1') process.stderr.write(JSON.stringify({ rpc: true, pid: process.pid, connection, direction, value }) + '\n');
}
function text(params: Record<string, unknown>, field: string): string {
  const value = params[field];
  if (typeof value !== 'string' || !value.length) throw new Fault('fatal');
  return value;
}

export class AuthQueue {
  private queue: Promise<unknown> = Promise.resolve();
  run(work: () => Promise<unknown>) {
    const next = this.queue.then(work);
    this.queue = next.catch(() => {});
    return next;
  }
  ready() { return this.queue; }
}

export async function serve(auth: Auth, getDrive: () => Promise<Drive>, close: () => void, write: (value: unknown) => void, input: Readable = process.stdin, authQueue = new AuthQueue()) {
  const connection = ++connectionNumber;
  const lines = createInterface({ input, crlfDelay: Infinity });
  const inflight = new Set<Promise<void>>();
  let walkQueue: Promise<unknown> = Promise.resolve();
  const handle = async (line: string) => {
    let request: any;
    try { request = JSON.parse(line); }
    catch { write({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error', data: { code: 'fatal' } } }); return; }
    if (!request || Array.isArray(request) || request.jsonrpc !== '2.0' || typeof request.method !== 'string' || (request.id !== undefined && request.id !== null && typeof request.id !== 'string' && typeof request.id !== 'number')) {
      write({ jsonrpc: '2.0', id: null, error: { code: -32600, message: 'Invalid Request', data: { code: 'fatal' } } }); return;
    }
    const fields = traceFields[request.method];
    if (fields) rpcTrace(connection, 'request', { id: request.id, method: request.method, params: Object.fromEntries(fields.filter(k => request.params?.[k] !== undefined).map(k => [k, request.params[k]])) });
    else rpcTrace(connection, 'request', { id: request.id, method: request.method.startsWith('auth.') ? 'auth' : 'unknown' });
    const respond = (body: object) => {
      if ('id' in request) {
        const value = { jsonrpc: '2.0', id: request.id, ...body };
        if (fields) rpcTrace(connection, 'response', value);
        write(value);
      }
    };
    const run = async () => {
      const p = request.params ?? {};
      if (!p || typeof p !== 'object' || Array.isArray(p)) throw new Fault('fatal');
      switch (request.method) {
        case 'auth.login': {
          if (p.hv_token === undefined) return auth.login(text(p, 'username'), text(p, 'password'));
          const type = p.hv_type ?? 'captcha';
          if (typeof type !== 'string' || !hvTypes.includes(type)) throw new Fault('fatal');
          return auth.login(text(p, 'username'), text(p, 'password'), { token: text(p, 'hv_token'), type: type as HvType });
        }
        case 'auth.submit_2fa': return auth.submit2fa(text(p, 'code'));
        case 'auth.submit_mailbox_password': return auth.submitMailbox(text(p, 'password'));
        case 'auth.status': await auth.rehydrate(); return auth.status();
        case 'auth.logout': return auth.logout();
        case 'node.resolve': return (await getDrive()).resolve(text(p, 'path'));
        case 'node.path': return (await getDrive()).path(text(p, 'uid'));
        case 'events.ack': return (await getDrive()).ack(text(p, 'scope_id'), text(p, 'event_id'));
        case 'node.list': return (await getDrive()).list(text(p, 'uid'));
        case 'node.create_folder': return (await getDrive()).writes().createFolder(text(p, 'parent_uid'), text(p, 'name'));
        case 'file.upload': return (await getDrive()).writes().upload(text(p, 'parent_uid'), text(p, 'name'), text(p, 'local_path'), request.id, p.replace_uid === undefined ? undefined : text(p, 'replace_uid'));
        case 'file.download': return (await getDrive()).writes().download(text(p, 'uid'), text(p, 'local_path'), request.id);
        case 'node.rename': return (await getDrive()).writes().rename(text(p, 'uid'), text(p, 'new_name'));
        case 'node.move': return (await getDrive()).writes().move(text(p, 'uid'), text(p, 'new_parent_uid'));
        case 'node.trash': {
          if (!Array.isArray(p.uids) || !p.uids.length || !p.uids.every((uid: unknown) => typeof uid === 'string' && uid.length)) throw new Fault('fatal');
          return (await getDrive()).writes().trash(p.uids);
        }
        case 'node.walk': {
          if (!Array.isArray(p.exclude_globs) || !p.exclude_globs.every((g: unknown) => typeof g === 'string')) throw new Fault('fatal');
          return (await getDrive()).walk(text(p, 'uid'), p.exclude_globs);
        }
        case 'events.subscribe': {
          if (p.since_event_id !== undefined && typeof p.since_event_id !== 'string') throw new Fault('fatal');
          if (p.ack_required !== undefined && typeof p.ack_required !== 'boolean') throw new Fault('fatal');
          return (await getDrive()).subscribe(text(p, 'scope_id'), p.since_event_id, p.ack_required);
        }
        default: throw new Fault('fatal');
      }
    };
    try {
      let result: unknown;
      if (request.method.startsWith('auth.')) {
        result = await authQueue.run(run);
      } else {
        await authQueue.ready();
        if (request.method === 'node.walk') {
          const next = walkQueue.then(run);
          walkQueue = next.catch(() => {});
          result = await next;
        } else result = await run();
      }
      respond({ result });
    } catch (error) {
      // Opt-in diagnostics for unexpected (non-Fault) errors: name and stack frames only, no message text.
      if (process.env.NEUTRONSYNC_DRIVE_DEBUG === '1' && !(error instanceof Fault)) {
        const e = error as { name?: string; stack?: string } | null;
        const frames = (e?.stack ?? '').split('\n').filter(l => l.trim().startsWith('at ')).slice(0, 4).map(l => l.trim());
        process.stderr.write(JSON.stringify({ debug: 'error', method: request.method, name: e?.name ?? typeof error, frames }) + '\n');
      }
      const fault = classify(error);
      respond({ error: { code: codes[fault.code], message: fault.message, data: { code: fault.code, ...(fault.retry_after === undefined ? {} : { retry_after: fault.retry_after }) } } });
    }
  };
  for await (const line of lines) {
    const work = handle(line);
    inflight.add(work);
    void work.finally(() => inflight.delete(work));
  }
  await Promise.all(inflight);
  close();
}

export function notifications(write: (value: unknown) => void): Notify {
  return (method, params) => write({ jsonrpc: '2.0', method, params });
}
