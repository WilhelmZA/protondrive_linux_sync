import { dlopen, FFIType, ptr } from 'bun:ffi';
import { closeSync, constants, fstatSync, lstatSync, mkdirSync, openSync, unlinkSync } from 'node:fs';
import { dirname } from 'node:path';
import { createServer, type Socket } from 'node:net';

const libc = dlopen('libc.so.6', {
  flock: { args: [FFIType.i32, FFIType.i32], returns: FFIType.i32 },
  getsockopt: { args: [FFIType.i32, FFIType.i32, FFIType.i32, FFIType.ptr, FFIType.ptr], returns: FFIType.i32 },
});

export function privateDirectory(path: string) {
  mkdirSync(path, { recursive: true, mode: 0o700 });
  const stat = lstatSync(path);
  if (!stat.isDirectory() || stat.uid !== process.getuid!() || (stat.mode & 0o777) !== 0o700) throw new Error('Unsafe socket directory');
}

// Keep this descriptor open for the entire session-owner lifetime. Never unlink
// the lock: contenders must always flock the same inode, including after a crash.
export function claimSocket(path: string): number | null {
  privateDirectory(dirname(path));
  const fd = openSync(path + '.lock', constants.O_CREAT | constants.O_RDWR | constants.O_NOFOLLOW, 0o600);
  const stat = fstatSync(fd);
  if (!stat.isFile() || stat.uid !== process.getuid!() || (stat.mode & 0o777) !== 0o600) { closeSync(fd); throw new Error('Unsafe socket lock'); }
  if (libc.symbols.flock(fd, 2 | 4) !== 0) { closeSync(fd); return null; }
  try { unlinkSync(path); } catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') { closeSync(fd); throw error; } }
  return fd;
}

export function sameUser(socket: Socket): boolean {
  // Bun's pinned node:net implementation exposes the native descriptor here.
  // Fail closed if that implementation changes.
  const fd = (socket as Socket & { _handle?: { fd?: number } })._handle?.fd;
  if (typeof fd !== 'number' || fd < 0) return false;
  const credentials = new Uint32Array(3);
  const length = new Uint32Array([credentials.byteLength]);
  return libc.symbols.getsockopt(fd, 1, 17, ptr(credentials), ptr(length)) === 0
    && length[0] === 12 && credentials[1] === process.getuid!(); // SOL_SOCKET, SO_PEERCRED
}

export async function listenSocket(path: string, connected: (socket: Socket) => void) {
  const server = createServer(socket => {
    if (!sameUser(socket)) { socket.destroy(); return; }
    socket.on('error', () => socket.destroy());
    connected(socket);
  });
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(path, resolve);
  });
  return server;
}
