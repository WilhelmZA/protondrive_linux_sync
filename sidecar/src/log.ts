export type Notify = (method: string, params: unknown) => void;

// Fail closed: arbitrary diagnostic text and objects never cross this boundary.
// This also covers secrets that a future caller has not registered for redaction.
const safeMessages = new Set([
  'Sidecar ready', 'SDK diagnostic suppressed', 'Diagnostic suppressed',
  'Session storage unavailable', 'Event polling failed', 'Folder listing failed',
  'My-files event scope follows in events.batch',
]);

export class SafeLog {
  constructor(private sink: (line: string) => void) {}
  write(level: string, message: unknown, ..._details: unknown[]) {
    const msg = typeof message === 'string' && safeMessages.has(message) ? message : 'Diagnostic suppressed';
    this.sink(JSON.stringify({ level: ['debug', 'info', 'warn', 'error'].includes(level) ? level : 'info', msg }));
  }
  installConsole() {
    for (const name of ['log', 'info', 'debug', 'warn', 'error', 'trace', 'dir', 'table'] as const) {
      console[name] = (...args: unknown[]) => this.write(name, ...([args[0], ...args.slice(1)] as [unknown, ...unknown[]]));
    }
  }
}
