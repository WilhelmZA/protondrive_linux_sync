export type Notify = (method: string, params: unknown) => void;

// Fail closed: arbitrary diagnostic text and objects never cross this boundary.
// This also covers secrets that a future caller has not registered for redaction.
const safeMessages = new Set([
  'Sidecar ready', 'SDK diagnostic suppressed', 'Diagnostic suppressed',
  'Session storage unavailable', 'Event polling failed', 'Folder listing failed',
  'My-files event scope follows in events.batch',
]);

// NEUTRONSYNC_DRIVE_DEBUG=1 (local, opt-in): show the first 200 characters of string messages.
const debug = process.env.NEUTRONSYNC_DRIVE_DEBUG === '1';

export class SafeLog {
  constructor(private sink: (line: string) => void) {}
  write(level: string, message: unknown, ..._details: unknown[]) {
    const lvl = ['debug', 'info', 'warn', 'error'].includes(level) ? level : 'info';
    const known = typeof message === 'string' && safeMessages.has(message);
    // Without debug, debug-level noise is dropped instead of flooding stderr.
    if (!debug && lvl === 'debug') return;
    const msg = known ? message : debug && typeof message === 'string' ? 'raw: ' + message.slice(0, 200) : 'Diagnostic suppressed';
    this.sink(JSON.stringify({ level: lvl, msg }));
  }
  sdk(level: string, message: unknown) { this.write(level, debug ? message : 'SDK diagnostic suppressed'); }
  installConsole() {
    for (const name of ['log', 'info', 'debug', 'warn', 'error', 'trace', 'dir', 'table'] as const) {
      console[name] = (...args: unknown[]) => this.write(name, ...([args[0], ...args.slice(1)] as [unknown, ...unknown[]]));
    }
  }
}
