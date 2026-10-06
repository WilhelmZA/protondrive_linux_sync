import { getSrp, computeKeyPassword } from '@protontech/crypto/srp';
import { Auth, type AuthCrypto } from './auth';
import { Account } from './account';
import { Drive, sdkClient, type ReadClient } from './drive';
import { SafeLog } from './log';
import { SecretServiceStore, type SessionStore } from './store';
import { notifications, serve } from './rpc';

const write = (value: unknown) => { process.stdout.write(JSON.stringify(value) + '\n'); };
const notify = notifications(write);
const log = new SafeLog(line => process.stderr.write(line + '\n'));
log.installConsole();

async function main() {
  // Verify the bundled SDK WASM is available independently of node_modules.
  const wasm = Bun.embeddedFiles.find(file => (file as File).name.endsWith('proton_drive_sdk_search_bg.wasm'));
  if (!wasm) throw new Error();
  await WebAssembly.compile(await wasm.arrayBuffer());
  let store: SessionStore = new SecretServiceStore();
  let crypto: AuthCrypto = { proof: getSrp, keyPassword: computeKeyPassword, unlock: async (_auth, password) => { await account.unlock(password); } };
  let fakeClient: ((auth: Auth) => ReadClient) | undefined;
  const args = process.argv.slice(2);
  if (args.length) {
    if (args.length !== 2 || args[0] !== '--test-backend') throw new Error();
    const { testDependencies } = await import('../tests/binary-backend');
    const test = testDependencies(args[1]!);
    store = test.store;
    crypto = test.crypto;
    fakeClient = test.client;
  }
  const auth = new Auth(store, crypto, notify, log, fetch, fakeClient ? args[1]! : undefined);
  let account = new Account(auth);
  // Instantiate the actual SDK even in the packaging test, without issuing a request.
  let client = sdkClient(auth, account, log);
  let drive: Drive | undefined;
  let opening: Promise<Drive> | undefined;
  auth.onClear = () => {
    drive?.close(); drive = undefined; opening = undefined;
    account = new Account(auth);
    client = sdkClient(auth, account, log);
  };
  await auth.init();
  const getDrive = async () => {
    const session = auth.requireSession();
    if (drive) return drive;
    if (!opening) opening = (async () => {
      if (!fakeClient) await account.unlock(session.keyPassphrase);
      if (auth.requireSession().uid !== session.uid) throw new Error();
      drive = new Drive(fakeClient ? fakeClient(auth) : client, notify, log);
      return drive;
    })().catch(error => { opening = undefined; throw error; });
    return opening;
  };
  notify('log', { level: 'info', msg: 'Sidecar ready' });
  const close = () => drive?.close();
  process.on('SIGTERM', () => { close(); process.exit(0); });
  process.on('SIGINT', () => { close(); process.exit(0); });
  await serve(auth, getDrive, close, write);
}

main().catch(() => { log.write('error', 'Diagnostic suppressed'); process.exitCode = 1; });
