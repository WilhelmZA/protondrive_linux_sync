import { expect, test } from 'bun:test';
import { Auth } from '../src/auth';
import { Account } from '../src/account';
import { Drive, sdkClient } from '../src/drive';
import { classify } from '../src/errors';
import { SafeLog } from '../src/log';
import { MemoryStore, fakeCrypto } from './binary-backend';
import { fixture } from './fixture';

test('actual SDK propagates missing and unauthorized My-files reads without creating a volume', async () => {
  for (const status of [404, 403]) {
    const backend = fixture();
    const sdkRequests: { path: string; method: string }[] = [];
    const log = new SafeLog(() => {});
    const auth = new Auth(new MemoryStore(), fakeCrypto, () => {}, log, async (url, init) => {
      const path = new URL(url).pathname;
      if (path.startsWith('/drive/')) {
        sdkRequests.push({ path, method: init.method ?? 'GET' });
        expect(new Headers(init.headers).get('x-pm-appversion')).toBe('external-drive-neutronsync@0.1.0-dev');
        return Response.json({ Code: 2501 }, { status });
      }
      return backend.fetch(url, init);
    });
    await auth.login('normal', 'password-secret');
    const client = sdkClient(auth, new Account(auth), log);
    const drive = new Drive(client, () => {}, log);
    try {
      await drive.list('volume~missing');
      throw new Error('Unexpected successful listing');
    } catch (error) {
      expect(classify(error).code).toBe(status === 404 ? 'not_found' : 'auth');
    }
    expect(sdkRequests.length).toBeGreaterThan(0);
    expect(sdkRequests.every(request => request.method === 'GET')).toBe(true);
  }
}, 10_000);
