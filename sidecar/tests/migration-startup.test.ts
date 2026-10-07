import { expect, test } from 'bun:test';
import { chmodSync, existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { connect } from 'node:net';
import { createInterface } from 'node:readline';
import { binary } from './compiled';

const request = JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'auth.status', params: {} }) + '\n';

async function socketStatus(path: string) {
  return new Promise<any>((resolve, reject) => {
    const socket = connect(path);
    const timer = setTimeout(() => { socket.destroy(); reject(new Error('status timeout')); }, 3000);
    socket.on('error', error => { clearTimeout(timer); reject(error); });
    const lines = createInterface({ input: socket });
    lines.on('line', line => {
      const value = JSON.parse(line);
      if (value.id === 1) { clearTimeout(timer); socket.destroy(); lines.close(); resolve(value.result); }
    });
    socket.on('connect', () => socket.write(request));
  });
}

test('compiled startup refuses failed migration on both transports and restarts signed in from preserved legacy data', async () => {
  for (const transport of ['stdio', 'socket']) {
    for (const failure of ['write', 'readback', 'lookup']) {
      const directory = mkdtempSync(join(tmpdir(), 'ns-migration-'));
      const path = join(directory, 'session.sock');
      // This synthetic secret-tool is the only store the subprocess can reach.
      // Marker files contain state names, never real keyring data.
      const tool = join(directory, 'secret-tool');
      writeFileSync(tool, `#!/usr/bin/env python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
recover = (root / 'recover').exists()
operation = sys.argv[1]
current = sys.argv[-1] == 'session-v1'
item = root / 'new-item'
session = dict(uid='synthetic-uid', account='migration-test', accessToken='synthetic-access', refreshToken='synthetic-refresh', keyPassphrase='synthetic-key', expiresAt=9999999999999)
if operation == 'lookup':
    if current and '${failure}' == 'lookup' and not recover:
        sys.stderr.write('storage unavailable\\n')
        sys.exit(1)
    if current:
        if not item.exists(): sys.exit(1)
        if item.read_text() == 'invalid':
            print('{}')
            sys.exit(0)
    elif (root / 'old-cleared').exists(): sys.exit(1)
    print(json.dumps(session))
elif operation == 'store':
    sys.stdin.read()
    if '${failure}' == 'write' and not recover:
        sys.stderr.write('storage unavailable\\n')
        sys.exit(1)
    item.write_text('invalid' if '${failure}' == 'readback' and not recover else 'valid')
elif operation == 'clear':
    if not current: (root / 'old-cleared').touch()
else: sys.exit(2)
`);
      chmodSync(tool, 0o700);
      const start = () => Bun.spawn([binary, ...(transport === 'socket' ? ['--socket', path] : [])], {
        env: { ...process.env, PATH: directory + ':' + process.env.PATH },
        stdin: new Blob([request]), stdout: 'pipe', stderr: 'pipe',
      });
      let child = start();
      let deadline = setTimeout(() => child.kill('SIGKILL'), 5000);
      try {
        const [status, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
        clearTimeout(deadline);
        expect(status).toBe(1);
        expect(stdout).toBe('');
        expect(existsSync(path)).toBe(false);
        expect(existsSync(join(directory, 'old-cleared'))).toBe(false);
        expect(stderr).not.toContain('synthetic-access');
        expect(stderr).not.toContain('synthetic-refresh');
        writeFileSync(join(directory, 'recover'), '');
        child = start();
        deadline = setTimeout(() => child.kill('SIGKILL'), 5000);
        if (transport === 'socket') {
          for (let i = 0; i < 100 && !existsSync(path); i++) await Bun.sleep(20);
          expect(await socketStatus(path)).toEqual({ signed_in: true, account: 'migration-test', keyring_locked: false });
        } else {
          const output = await new Response(child.stdout).text();
          expect(await child.exited).toBe(0);
          const status = output.trim().split('\n').map(line => JSON.parse(line)).find(value => value.id === 1);
          expect(status.result).toEqual({ signed_in: true, account: 'migration-test', keyring_locked: false });
          expect(output).not.toContain('auth.signed_out');
        }
        expect(existsSync(join(directory, 'old-cleared'))).toBe(true);
      } finally {
        clearTimeout(deadline);
        child.kill(); await child.exited;
        rmSync(directory, { recursive: true, force: true });
      }
    }
  }
}, 30000);
