import type { components } from "./schema";

export type Problem = components["schemas"]["Problem"];

export class ApiError extends Error {
  readonly problem: Problem;
  readonly status: number;

  constructor(problem: Problem) {
    super(problem.detail ?? problem.title ?? "Request failed");
    this.problem = problem;
    this.status = problem.status;
  }

  get code(): string | undefined {
    return this.problem.code;
  }
}

interface Session {
  accessToken: string | null;
  tenantId: string | null;
  facilityId: string | null;
}

/** Mutable module singleton — read fresh on every request, never captured in a stale closure. */
const session: Session = { accessToken: null, tenantId: null, facilityId: null };

export function setSession(patch: Partial<Session>): void {
  Object.assign(session, patch);
}

export function clearSession(): void {
  session.accessToken = null;
  session.tenantId = null;
  session.facilityId = null;
}

type RefreshHandler = () => Promise<boolean>;
let refreshHandler: RefreshHandler | null = null;
export function setRefreshHandler(fn: RefreshHandler | null): void {
  refreshHandler = fn;
}

/** In-flight refresh, shared across concurrent 401s — the server rotates and
 *  invalidates the refresh token atomically on use, so a second concurrent call
 *  with the same (now-stale) token would 401 and log the user out from under
 *  a refresh that actually just succeeded. */
let refreshInFlight: Promise<boolean> | null = null;
function runRefreshOnce(): Promise<boolean> {
  if (!refreshInFlight) {
    refreshInFlight = refreshHandler!().finally(() => {
      refreshInFlight = null;
    });
  }
  return refreshInFlight;
}

type SessionExpiredHandler = () => void;
let onSessionExpired: SessionExpiredHandler | null = null;
export function setOnSessionExpired(fn: SessionExpiredHandler | null): void {
  onSessionExpired = fn;
}

const BASE_URL = import.meta.env.VITE_API_BASE_URL ?? "http://localhost:8080/api/v1";

interface RequestOptions extends Omit<RequestInit, "body"> {
  /** Set false for pre-login calls (token issuance, refresh) that must not send stale credentials. */
  auth?: boolean;
  body?: unknown;
  /** Resource's current `version` — required by every PATCH/transition on a versioned resource. */
  ifMatch?: number | string;
  /** Client-generated key for the 13 mutating POSTs the contract requires it on — see docs/FRONTEND-GETTING-STARTED.md §4.3. */
  idempotencyKey?: string;
}

function buildHeaders(opts: RequestOptions): Headers {
  const headers = new Headers(opts.headers);
  headers.set("X-Request-ID", crypto.randomUUID());
  if (opts.auth !== false) {
    if (session.accessToken) headers.set("Authorization", `Bearer ${session.accessToken}`);
    if (session.tenantId) headers.set("X-Tenant-ID", session.tenantId);
    if (session.facilityId) headers.set("X-Facility-ID", session.facilityId);
  }
  if (opts.ifMatch !== undefined) headers.set("If-Match", String(opts.ifMatch));
  if (opts.idempotencyKey) headers.set("Idempotency-Key", opts.idempotencyKey);
  if (opts.body !== undefined && !(opts.body instanceof FormData) && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }
  return headers;
}

/** One per create-attempt, reused across retries of that same attempt — never regenerated on retry. */
export function newIdempotencyKey(): string {
  return crypto.randomUUID();
}

async function rawFetch(path: string, opts: RequestOptions): Promise<Response> {
  const body = opts.body === undefined ? undefined : opts.body instanceof FormData ? opts.body : JSON.stringify(opts.body);
  return fetch(`${BASE_URL}${path}`, {
    ...opts,
    headers: buildHeaders(opts),
    body,
  });
}

async function toApiError(res: Response): Promise<ApiError> {
  try {
    const problem = (await res.json()) as Problem;
    return new ApiError(problem);
  } catch {
    return new ApiError({ type: "about:blank", title: res.statusText, status: res.status });
  }
}

/**
 * Core request helper: injects auth/tenant/facility/request-id headers, retries once
 * after a successful silent refresh on 401, and throws ApiError with the parsed
 * RFC 9457 problem+json body on any non-2xx response.
 */
export async function apiFetch<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  let res = await rawFetch(path, opts);

  if (res.status === 401 && opts.auth !== false && refreshHandler) {
    const refreshed = await runRefreshOnce();
    if (refreshed) {
      res = await rawFetch(path, opts);
    }
  }

  if (!res.ok) {
    const error = await toApiError(res);
    if (error.code === "UNAUTHENTICATED" && onSessionExpired) onSessionExpired();
    throw error;
  }

  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return text ? (JSON.parse(text) as T) : (undefined as T);
}
