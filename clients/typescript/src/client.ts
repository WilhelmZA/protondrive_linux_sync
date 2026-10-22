/**
 * Thin HTTP client for `neutronsync serve` and the desktop tray.
 *
 * TCP: `baseUrl` is the origin, for example `http://127.0.0.1:8090`.
 * Unix socket: set `socketPath`. `baseUrl` is only the Host header
 * (`http://localhost` or `http://x`). Bytes go to the socket via
 * `http.request({ socketPath })`. No `Origin` or `Sec-Fetch-Site` header.
 *
 * Response JSON is parsed and returned as-is. Fields this client does not
 * know about stay on the object and are not rejected. v1 may add optional
 * fields; request bodies still may not.
 */

import http from "node:http";
import https from "node:https";

import type {
  Accepted,
  AccountState,
  ApiVersion,
  AuthStep,
  AutoSyncBody,
  AutoSyncReply,
  CodeBody,
  ConfirmBody,
  ConflictRecord,
  Envelope,
  Health,
  HeldDeletes,
  LocalEntry,
  LogLines,
  LoginBody,
  PairIn,
  PairOut,
  PairRemoved,
  PasswordBody,
  RemoteEntry,
  Settings,
  SyncBody,
} from "./openapi.ts";

const DEFAULT_TIMEOUT_MS = 10_000;
const SIGN_IN_TIMEOUT_MS = 60_000;

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(code: string, message: string, status = 0) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.status = status;
  }
}

export type ClientOptions = {
  /** Origin. On a unix socket this is only the Host header. */
  baseUrl: string;
  /** Sent as `Authorization: Bearer` when set. Omit on the desktop socket. */
  token?: string;
  /** Unix socket path. When set, the connection is not TCP. */
  socketPath?: string;
};

export function createClient(options: ClientOptions): Client {
  return new Client(options);
}

export class Client {
  readonly baseUrl: string;
  /** Set when requests go to a unix socket instead of TCP. */
  readonly socketPath: string | undefined;
  private readonly token: string | undefined;
  private readonly origin: URL;

  constructor(options: ClientOptions) {
    let origin: URL;
    try {
      origin = new URL(options.baseUrl);
    } catch {
      throw new Error(`baseUrl is not an absolute URL: ${options.baseUrl}`);
    }
    if (origin.protocol !== "http:" && origin.protocol !== "https:") {
      throw new Error(`baseUrl must be http or https, got ${origin.protocol}`);
    }
    const socketPath = options.socketPath || undefined;
    if (socketPath && origin.protocol !== "http:") {
      throw new Error("A unix socket uses an http baseUrl; it is only the Host header");
    }
    this.origin = origin;
    this.baseUrl = options.baseUrl;
    this.socketPath = socketPath;
    this.token = options.token || undefined;
  }

  /** One HTTP call. `path` is absolute (`/api/v1/...`) and may include a query. */
  request<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
    return this.requestTimeout(method, path, body, DEFAULT_TIMEOUT_MS);
  }

  health(): Promise<Health> {
    return this.request<Health>("GET", "/api/v1/health");
  }

  version(): Promise<ApiVersion> {
    return this.request<ApiVersion>("GET", "/api/v1/version");
  }

  state(activity = 100): Promise<Envelope> {
    return this.request<Envelope>("GET", withQuery("/api/v1/state", { activity }));
  }

  sync(body: SyncBody = {}): Promise<Accepted> {
    return this.request<Accepted>("POST", "/api/v1/sync", body);
  }

  cancel(): Promise<Accepted> {
    return this.request<Accepted>("POST", "/api/v1/cancel");
  }

  reset(body: ConfirmBody): Promise<Accepted> {
    return this.request<Accepted>("POST", "/api/v1/reset", body);
  }

  autoSync(body: AutoSyncBody): Promise<AutoSyncReply> {
    return this.request<AutoSyncReply>("POST", "/api/v1/auto-sync", body);
  }

  held(): Promise<HeldDeletes[]> {
    return this.request<HeldDeletes[]>("GET", "/api/v1/held");
  }

  heldRestore(pair: string): Promise<Accepted> {
    return this.request<Accepted>("POST", `/api/v1/held/${encodeURIComponent(pair)}/restore`);
  }

  heldDelete(pair: string, body: ConfirmBody): Promise<Accepted> {
    return this.request<Accepted>("POST", `/api/v1/held/${encodeURIComponent(pair)}/delete`, body);
  }

  conflicts(): Promise<ConflictRecord[]> {
    return this.request<ConflictRecord[]>("GET", "/api/v1/conflicts");
  }

  conflictDismiss(id: number): Promise<Accepted> {
    return this.request<Accepted>("POST", `/api/v1/conflicts/${encodeURIComponent(String(id))}/dismiss`);
  }

  conflictsDismissAll(): Promise<Accepted> {
    return this.request<Accepted>("POST", "/api/v1/conflicts/dismiss-all");
  }

  settings(): Promise<Settings> {
    return this.request<Settings>("GET", "/api/v1/settings");
  }

  settingsPut(body: Settings): Promise<Settings> {
    return this.request<Settings>("PUT", "/api/v1/settings", body);
  }

  pairs(): Promise<PairOut[]> {
    return this.request<PairOut[]>("GET", "/api/v1/pairs");
  }

  pairsAdd(body: PairIn): Promise<PairOut[]> {
    return this.request<PairOut[]>("POST", "/api/v1/pairs", body);
  }

  pairsReplace(name: string, body: PairIn): Promise<PairOut[]> {
    return this.request<PairOut[]>("PUT", `/api/v1/pairs/${encodeURIComponent(name)}`, body);
  }

  pairsRemove(name: string): Promise<PairRemoved> {
    return this.request<PairRemoved>("DELETE", `/api/v1/pairs/${encodeURIComponent(name)}`);
  }

  account(): Promise<AccountState> {
    return this.request<AccountState>("GET", "/api/v1/account");
  }

  login(body: LoginBody): Promise<AuthStep> {
    return this.requestTimeout<AuthStep>("POST", "/api/v1/account/login", body, SIGN_IN_TIMEOUT_MS);
  }

  twoFactor(body: CodeBody): Promise<AuthStep> {
    return this.requestTimeout<AuthStep>("POST", "/api/v1/account/2fa", body, SIGN_IN_TIMEOUT_MS);
  }

  mailboxPassword(body: PasswordBody): Promise<AuthStep> {
    return this.requestTimeout<AuthStep>(
      "POST",
      "/api/v1/account/mailbox-password",
      body,
      SIGN_IN_TIMEOUT_MS,
    );
  }

  logout(): Promise<AuthStep> {
    return this.request<AuthStep>("POST", "/api/v1/account/logout");
  }

  remoteList(path?: string): Promise<RemoteEntry[]> {
    return this.request<RemoteEntry[]>("GET", withQuery("/api/v1/remote/list", { path }));
  }

  localList(path?: string): Promise<LocalEntry[]> {
    return this.request<LocalEntry[]>("GET", withQuery("/api/v1/local/list", { path }));
  }

  logs(lines = 200): Promise<LogLines> {
    return this.request<LogLines>("GET", withQuery("/api/v1/logs", { lines }));
  }

  openapi(): Promise<unknown> {
    return this.request<unknown>("GET", "/api/v1/openapi.json");
  }

  /** `GET /api/v1/events`. `followEvents` reads this stream. */
  async openEvents(): Promise<EventResponse> {
    const res = await this.send("GET", "/api/v1/events", undefined, {
      accept: "text/event-stream",
      cacheControl: "no-store",
      timeoutMs: 0,
    });
    const status = res.statusCode ?? 0;
    if (status >= 400) {
      throw await errorFromResponse(res);
    }
    return {
      status,
      contentType: String(res.headers["content-type"] ?? ""),
      body: res,
    };
  }

  private requestTimeout<T>(method: string, path: string, body: unknown, timeoutMs: number): Promise<T> {
    return this.send(method, path, body, { accept: "application/json", timeoutMs }).then(readJson<T>);
  }

  private send(
    method: string,
    path: string,
    body: unknown,
    opts: { accept: string; timeoutMs: number; cacheControl?: string },
  ): Promise<http.IncomingMessage> {
    const payload = body === undefined ? undefined : JSON.stringify(body);
    const headers: http.OutgoingHttpHeaders = {
      Host: this.origin.host,
      Accept: opts.accept,
    };
    if (opts.cacheControl) headers["Cache-Control"] = opts.cacheControl;
    if (this.token) headers.Authorization = `Bearer ${this.token}`;
    if (payload !== undefined) {
      headers["Content-Type"] = "application/json";
      headers["Content-Length"] = Buffer.byteLength(payload);
    }

    const requestOptions: http.RequestOptions = this.socketPath
      ? { socketPath: this.socketPath, agent: false, method, path, headers }
      : {
          protocol: this.origin.protocol,
          hostname: this.origin.hostname,
          port: this.origin.port || undefined,
          agent: false,
          method,
          path,
          headers,
        };
    const lib = !this.socketPath && this.origin.protocol === "https:" ? https : http;

    return new Promise((resolve, reject) => {
      let settled = false;
      const fail = (err: unknown) => {
        if (settled) return;
        settled = true;
        if (err instanceof ApiError) {
          reject(err);
          return;
        }
        const message = err instanceof Error ? err.message : String(err);
        reject(new ApiError("unreachable", `Cannot reach the NeutronSync engine: ${message}`));
      };
      const req = lib.request(requestOptions, (res) => {
        if (settled) {
          res.resume();
          return;
        }
        settled = true;
        resolve(res);
      });
      req.on("error", fail);
      if (opts.timeoutMs > 0) {
        req.setTimeout(opts.timeoutMs, () => {
          req.destroy(new ApiError("unreachable", "Timed out talking to the NeutronSync engine"));
        });
      }
      if (payload !== undefined) req.write(payload);
      req.end();
    });
  }
}

export type EventResponse = {
  status: number;
  contentType: string;
  body: AsyncIterable<Uint8Array> & { destroy(): void };
};

async function readJson<T>(res: http.IncomingMessage): Promise<T> {
  const status = res.statusCode ?? 0;
  if (status >= 400) throw await errorFromResponse(res);
  const raw = await readBody(res);
  if (status === 204 || raw.length === 0) return undefined as T;
  try {
    return JSON.parse(raw.toString("utf8")) as T;
  } catch {
    throw new ApiError("bad_response", "Response was not JSON", status);
  }
}

async function errorFromResponse(res: http.IncomingMessage): Promise<ApiError> {
  const status = res.statusCode ?? 0;
  const raw = await readBody(res);
  const text = raw.toString("utf8");
  try {
    const body = JSON.parse(text) as { error?: { code?: unknown; message?: unknown } };
    const code = typeof body.error?.code === "string" ? body.error.code : "error";
    const message =
      typeof body.error?.message === "string" && body.error.message
        ? body.error.message
        : text || `HTTP ${status}`;
    return new ApiError(code, message, status);
  } catch {
    return new ApiError("error", text || `HTTP ${status}`, status);
  }
}

async function readBody(stream: AsyncIterable<Uint8Array>): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of stream) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks);
}

function withQuery(path: string, params: Record<string, string | number | undefined>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined) search.set(key, String(value));
  }
  const text = search.toString();
  return text ? `${path}?${text}` : path;
}
