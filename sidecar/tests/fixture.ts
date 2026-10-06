import { appVersion } from '../src/auth';

export function fixture() {
  let account = 'normal';
  let refreshes = 0;
  let rejectRefresh = false;
  let rejectAccess = false;
  let refreshStatus: number | undefined;
  const requests: { path: string; method: string; version: string | null; token: string | null }[] = [];
  const root = { uid: 'root', type: 'folder', name: { ok: true, value: 'My files' }, treeEventScopeId: 'sdk-scope-id' };
  const folder = { uid: 'documents', type: 'folder', parentUid: 'root', name: { ok: true, value: 'Documents' }, treeEventScopeId: 'sdk-scope-id' };
  const file = { uid: 'file', type: 'file', parentUid: 'documents', name: { ok: true, value: 'sample.txt' }, treeEventScopeId: 'sdk-scope-id', activeRevision: { claimedSize: 42, claimedModificationTime: '2026-01-02T03:04:05Z', claimedDigests: { sha1: 'abc123', sha1Verified: true } } };
  const missingMetadata = { uid: 'no-metadata', type: 'file', parentUid: 'documents', name: { ok: true, value: 'unknown.txt' }, treeEventScopeId: 'sdk-scope-id' };
  const reply = (data: unknown, status = 200, headers?: Record<string, string>) => Response.json(data, { status, headers });
  const handle = async (request: Request): Promise<Response> => {
    const url = new URL(request.url);
    const path = url.pathname;
    requests.push({ path, method: request.method, version: request.headers.get('x-pm-appversion'), token: request.headers.get('authorization') });
    if (request.headers.get('x-pm-appversion') !== appVersion) return reply({}, 400);
    const body: any = request.method === 'POST' ? await request.json() : {};
    if (path === '/auth/v4/info') { account = body.Username; return reply({ Code: 1000, SRPSession: 'fake-session' }); }
    if (path === '/auth/v4') {
      if (body.ClientProof === 'bad-proof') return reply({ Error: 'bad-password' }, 401);
      return reply({ Code: 1000, UID: 'session-uid', AccessToken: 'access-secret', RefreshToken: 'refresh-secret', ServerProof: btoa(account === 'bad-server' ? 'wrong-proof' : 'fake-server-proof'), ExpiresIn: 3600, PasswordMode: account.includes('two-password') ? 2 : 1, '2FA': { Enabled: account.includes('totp') ? 1 : 0 } });
    }
    if (path === '/auth/v4/2fa') return body.TwoFactorCode === '123456' ? reply({ Code: 1000 }) : reply({ Error: body.TwoFactorCode }, 401);
    if (path === '/auth/v4/refresh') {
      refreshes++;
      await Bun.sleep(30);
      if (refreshStatus) return reply({}, refreshStatus);
      if (rejectRefresh) return reply({ error: 'invalid_grant', Error: 'refresh-secret' }, 400);
      return reply({ Code: 1000, AccessToken: 'rotated-access', RefreshToken: 'rotated-refresh', ExpiresIn: 3600 });
    }
    if (rejectAccess && request.headers.get('authorization') === 'Bearer access-secret') return reply({}, 401);
    if (path === '/core/v4/users') return reply({ User: { Keys: [{ ID: 'key-id', Primary: 1 }] } });
    if (path === '/core/v4/keys/salts') return reply({ KeySalts: [{ ID: 'key-id', KeySalt: 'fake-salt' }] });
    if (path === '/fixture/root') return reply(root);
    if (path.startsWith('/fixture/node/')) {
      const uid = decodeURIComponent(path.slice('/fixture/node/'.length));
      const errors: Record<string, number> = { missing: 404, denied: 403, conflict: 409, limited: 429, transient: 503, fatal: 422 };
      if (uid === 'expired') { rejectRefresh = true; return reply({}, 401); }
      if (errors[uid]) return reply({ Error: 'password-secret access-secret refresh-secret' }, errors[uid], { 'retry-after': '7' });
      const node = [root, folder, file, missingMetadata].find(node => node.uid === uid);
      return node ? reply(node) : reply({}, 404);
    }
    if (path.startsWith('/fixture/children/')) {
      const uid = path.slice('/fixture/children/'.length);
      return reply(uid === 'root' ? [folder] : uid === 'documents' ? [file, missingMetadata] : []);
    }
    if (path === '/fixture/events') {
      const scope = url.searchParams.get('scope');
      if (!url.searchParams.has('since')) return reply([{ type: 'fast_forward', treeEventScopeId: scope, eventId: 'event-0' }]);
      return reply([
        ...['node_created', 'node_updated', 'node_deleted'].map((type, i) => ({ type, nodeUid: 'file', parentNodeUid: 'documents', treeEventScopeId: scope, eventId: `event-${i + 1}` })),
        { type: 'tree_refresh', treeEventScopeId: scope, eventId: 'event-4' },
        { type: 'tree_remove', treeEventScopeId: scope, eventId: 'none' },
      ]);
    }
    return reply({}, 404);
  };
  return {
    handle, requests, root, folder, file, missingMetadata,
    fetch: (url: string, init: RequestInit) => handle(new Request(url, init)),
    get refreshes() { return refreshes; },
    failRefresh() { rejectRefresh = true; },
    expireAccess() { rejectAccess = true; },
    refreshFailure(status: number) { refreshStatus = status; },
  };
}
