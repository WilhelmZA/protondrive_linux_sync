import { resolve } from 'node:path';

// Tests must exercise this checkout, including in a clean CI workspace.
const build = Bun.spawn(['bash', resolve(import.meta.dir, '../build.sh')], { stdout: 'pipe', stderr: 'pipe' });
const [out, err, status] = await Promise.all([new Response(build.stdout).text(), new Response(build.stderr).text(), build.exited]);
if (status !== 0) throw new Error(`Sidecar build failed: ${out}${err}`);
export const binary = resolve(import.meta.dir, '../dist/neutronsync-drive');
