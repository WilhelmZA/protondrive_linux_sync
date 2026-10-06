import { mkdtemp, rename, rm, readdir } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import assert from 'node:assert/strict';

const root = resolve(import.meta.dir, '..');
const directory = await mkdtemp('/tmp/opencode/neutronsync-standalone-');
const modules = join(root, 'node_modules');
const hidden = join(directory, 'dependencies-aside');
const cwd = join(directory, 'empty');
await import('node:fs/promises').then(fs => fs.mkdir(cwd));
try {
  await rename(modules, hidden);
  const child = Bun.spawn([join(root, 'dist/neutronsync-drive')], {
    cwd, env: { PATH: '', HOME: directory },
    stdin: new Blob(['{"jsonrpc":"2.0","id":1,"method":"auth.status"}\n']),
    stdout: 'pipe', stderr: 'pipe',
  });
  const [stdout, stderr, status] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
  assert.equal(status, 0, stderr);
  const messages = stdout.trim().split('\n').map(line => JSON.parse(line));
  assert.deepEqual(messages.find(m => m.id === 1)?.result, { signed_in: false, account: null });
  assert.deepEqual(await readdir(cwd), []);
  assert.deepEqual(await readdir(join(root, 'dist')), ['neutronsync-drive']);
  process.stdout.write('Standalone passed: SDK WASM validated; node_modules moved aside; empty working directory; PATH empty.\n');
  process.stdout.write(stdout);
} finally {
  await rename(hidden, modules).catch(() => {});
  // Never remove the holding directory if restoring dependencies fails.
  if ((await readdir(directory)).includes('dependencies-aside')) throw new Error('Dependency restoration failed');
  await rm(directory, { recursive: true });
}
