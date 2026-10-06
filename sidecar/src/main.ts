import { getSrp, computeKeyPassword } from '@protontech/crypto/srp';
import { Auth, type AuthCrypto } from './auth';
import { Account } from './account';
import { Drive, sdkClient, type ReadClient } from './drive';
import { SafeLog } from './log';
import { SecretServiceStore, type SessionStore } from './store';
import { AuthQueue, notifications, serve } from './rpc';
import { claimSocket, listenSocket } from './socket';
import { realpathSync } from 'node:fs';

const write = (value: unknown) => { process.stdout.write(JSON.stringify(value) + '\n'); };
const log = new SafeLog(line => process.stderr.write(line + '\n'));
log.installConsole();

async function main() {
  const args = process.argv.slice(2);
  const socketPath = args[0] === '--socket' ? args[1] : undefined;
  if (socketPath) args.splice(0, 2);
  // Ownership precedes the first keyring read, including automatic migration.
  if (socketPath && claimSocket(socketPath) === null) return;
  const outputs = new Set<(value: unknown) => void>();
  const notify = notifications(value => { for (const output of outputs) output(value); });
  // Verify the bundled SDK WASM is available independently of node_modules.
  const wasm = Bun.embeddedFiles.find(file => (file as File).name.endsWith('proton_drive_sdk_search_bg.wasm'));
  if (!wasm) throw new Error();
  await WebAssembly.compile(await wasm.arrayBuffer());
  let store: SessionStore = new SecretServiceStore();
  let crypto: AuthCrypto = { proof: getSrp, keyPassword: computeKeyPassword, unlock: async (_auth, password) => { await account.unlock(password); } };
  let fakeClient: ((auth: Auth) => ReadClient) | undefined;
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
  const drives = new Set<Drive>();
  let opening: Promise<void> | undefined;
  auth.onClear = () => {
    for (const drive of drives) drive.close();
    drives.clear(); opening = undefined;
    account = new Account(auth);
    client = sdkClient(auth, account, log);
  };
  await auth.init();
  const queue = new AuthQueue();
  const connection = async (output: typeof write, input = process.stdin) => {
    outputs.add(output);
    notifications(output)('log', { level: 'info', msg: 'Sidecar ready' });
    let drive: Drive | undefined;
    let disconnected = false;
    const close = () => { disconnected = true; outputs.delete(output); if (drive) { drive.close(); drives.delete(drive); } };
    input.once('close', close);
    const getDrive = async () => {
      const session = auth.requireSession();
      if (drive && drives.has(drive)) return drive;
      if (!opening) opening = (async () => {
        if (!fakeClient) await account.unlock(session.keyPassphrase);
        if (auth.requireSession().uid !== session.uid) throw new Error();
      })().catch(error => { opening = undefined; throw error; });
      await opening;
      if (disconnected) throw new Error();
      // Per-connection Drive objects own independent event cursors and sinks.
      if (!drive || !drives.has(drive)) drive = new Drive(fakeClient ? fakeClient(auth) : client, notifications(output), log);
      drives.add(drive);
      return drive;
    };
    try { await serve(auth, getDrive, close, output, input, queue); }
    finally { close(); }
  };
  const close = () => { for (const drive of drives) drive.close(); };
  process.on('SIGTERM', () => { close(); process.exit(0); });
  process.on('SIGINT', () => { close(); process.exit(0); });
  if (socketPath) {
    await listenSocket(socketPath, socket => {
      const output = (value: unknown) => { if (!socket.destroyed) socket.write(JSON.stringify(value) + '\n'); };
      output({ jsonrpc: '2.0', method: 'sidecar.hello', params: { binary: realpathSync(process.execPath), pid: process.pid } });
      void connection(output, socket as unknown as typeof process.stdin).catch(() => socket.destroy());
    });
  } else {
    await connection(write);
  }
}

main().catch(() => { log.write('error', 'Diagnostic suppressed'); process.exitCode = 1; });
