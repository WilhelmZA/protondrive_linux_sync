export type ErrorCode = 'not_found' | 'auth' | 'conflict' | 'rate_limited' | 'transient' | 'fatal';

const messages: Record<ErrorCode, string> = {
  not_found: 'Node not found', auth: 'Authentication required or rejected',
  conflict: 'Conflicting operation', rate_limited: 'Rate limited',
  transient: 'Temporary service failure', fatal: 'Operation failed',
};

export class Fault extends Error {
  constructor(public code: ErrorCode, public retry_after?: number) {
    super(messages[code]);
  }
}

export function classify(error: unknown): Fault {
  if (error instanceof Fault) return error;
  const e = error as { statusCode?: number; name?: string; cause?: unknown } | null;
  if (e?.cause instanceof Fault) return e.cause;
  if (Array.isArray(e?.cause)) {
    const faults = e.cause.map(classify);
    return faults.find(f => f.code === 'auth') ?? faults.find(f => f.code !== 'fatal') ?? new Fault('fatal');
  }
  if (e?.statusCode) return httpFault(e.statusCode);
  if (e?.name === 'NodeWithSameNameExistsValidationError') return new Fault('conflict');
  if (e?.name === 'ConnectionError' || e?.name === 'TimeoutError') return new Fault('transient');
  return new Fault('fatal');
}

export function httpFault(status: number, retryAfter?: string | null): Fault {
  if (status === 404) return new Fault('not_found');
  if (status === 401 || status === 403) return new Fault('auth');
  if (status === 409) return new Fault('conflict');
  if (status === 429) {
    const numeric = Number(retryAfter);
    const seconds = retryAfter && Number.isFinite(numeric) ? numeric : (Date.parse(retryAfter ?? '') - Date.now()) / 1000;
    return new Fault('rate_limited', Number.isFinite(seconds) ? Math.max(0, Math.ceil(seconds)) : 60);
  }
  return new Fault(status >= 500 || status === 408 ? 'transient' : 'fatal');
}
