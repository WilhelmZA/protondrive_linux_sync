import { createInterface } from 'node:readline';
import { hvTypes, type Auth, type HvType } from './auth';
import type { Drive } from './drive';
import { Fault, classify } from './errors';
import type { Notify } from './log';

const codes = { not_found: -32004, auth: -32001, conflict: -32009, rate_limited: -32029, transient: -32050, fatal: -32603 };
function text(params: Record<string, unknown>, field: string): string {
  const value = params[field];
  if (typeof value !== 'string' || !value.length) throw new Fault('fatal');
  return value;
}

export async function serve(auth: Auth, getDrive: () => Promise<Drive>, close: () => void, write: (value: unknown) => void) {
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  const inflight = new Set<Promise<void>>();
  let authQueue: Promise<unknown> = Promise.resolve();
  const handle = async (line: string) => {
    let request: any;
    try { request = JSON.parse(line); }
    catch { write({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error', data: { code: 'fatal' } } }); return; }
    if (!request || Array.isArray(request) || request.jsonrpc !== '2.0' || typeof request.method !== 'string' || (request.id !== undefined && request.id !== null && typeof request.id !== 'string' && typeof request.id !== 'number')) {
      write({ jsonrpc: '2.0', id: null, error: { code: -32600, message: 'Invalid Request', data: { code: 'fatal' } } }); return;
    }
    const respond = (body: object) => { if ('id' in request) write({ jsonrpc: '2.0', id: request.id, ...body }); };
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
        case 'auth.status': return auth.status();
        case 'auth.logout': return auth.logout();
        case 'node.resolve': return (await getDrive()).resolve(text(p, 'path'));
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
          return (await getDrive()).subscribe(text(p, 'scope_id'), p.since_event_id);
        }
        default: throw new Fault('fatal');
      }
    };
    try {
      let result: unknown;
      if (request.method.startsWith('auth.')) {
        const next = authQueue.then(run);
        authQueue = next.catch(() => {});
        result = await next;
      } else { await authQueue; result = await run(); }
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
