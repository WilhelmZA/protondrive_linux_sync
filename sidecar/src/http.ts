import type { ProtonDriveHTTPClient } from '@protontech/drive-sdk';
import type { Auth } from './auth';
import { Fault } from './errors';

// SDK 0.22.2 metadata reads, folder creation, rename/move/trash and file uploads.
// Draft deletion is deliberately not authorised, even on failed uploads.
const postPaths = [
  /^\/drive\/v2\/volumes\/[^/]+\/links$/,
  /^\/drive\/v2\/volumes\/[^/]+\/folders$/,
  /^\/drive\/v2\/volumes\/[^/]+\/trash_multiple$/,
  /^\/drive\/v2\/volumes\/[^/]+\/links\/[^/]+\/checkAvailableHashes$/,
  /^\/drive\/v2\/volumes\/[^/]+\/files(?:\/small)?$/,
  /^\/drive\/v2\/volumes\/[^/]+\/files\/[^/]+\/revisions(?:\/small)?$/,
  /^\/drive\/blocks$/,
];
const putPaths = [
  /^\/drive\/v2\/volumes\/[^/]+\/links\/[^/]+\/(?:rename|move)$/,
  /^\/drive\/v2\/volumes\/[^/]+\/files\/[^/]+\/revisions\/[^/]+$/,
];

export function httpClient(auth: Auth): ProtonDriveHTTPClient {
  const signal = (r: { timeoutMs: number; signal?: AbortSignal }) => {
    const timeout = AbortSignal.timeout(r.timeoutMs || 30_000);
    return r.signal ? AbortSignal.any([r.signal, timeout]) : timeout;
  };
  return {
    async fetchJson(r) {
      const method = r.method.toUpperCase();
      const path = new URL(r.url).pathname;
      if (method !== 'GET' && !(method === 'POST' && postPaths.some(p => p.test(path))) && !(method === 'PUT' && putPaths.some(p => p.test(path)))) throw new Fault('fatal');
      return auth.request(r.url, { method, headers: r.headers, body: r.body ?? (r.json === undefined ? undefined : JSON.stringify(r.json)), signal: signal(r) }, true);
    },
    async fetchBlob(r) {
      if (!['GET', 'POST'].includes(r.method.toUpperCase())) throw new Fault('fatal');
      return auth.blob(r.url, { method: r.method, headers: r.headers, body: r.body, signal: signal(r) });
    },
  };
}
