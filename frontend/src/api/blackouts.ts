import { apiFetch, ApiError } from "./client";
import type { Problem } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type ResourceBlackout = components["schemas"]["ResourceBlackout"];
export type BlackoutType = NonNullable<ResourceBlackout["blackout_type"]>;

export interface ListBlackoutsParams {
  facilityId?: string;
  bookableResourceId?: string;
  from?: string;
  to?: string;
  cursor?: string;
  limit?: number;
}

export function listResourceBlackouts(params: ListBlackoutsParams): Promise<PagedEnvelope<ResourceBlackout>> {
  const query = new URLSearchParams();
  if (params.facilityId) query.set("facility_id", params.facilityId);
  if (params.bookableResourceId) query.set("bookable_resource_id", params.bookableResourceId);
  if (params.from) query.set("from", params.from);
  if (params.to) query.set("to", params.to);
  if (params.cursor) query.set("cursor", params.cursor);
  query.set("limit", String(params.limit ?? 50));
  return apiFetch<PagedEnvelope<ResourceBlackout>>(`/resource-blackouts?${query}`);
}

export interface CreateBlackoutBody {
  facility_id: string;
  bookable_resource_id?: string | null;
  start_at: string;
  end_at: string;
  reason: string;
  blackout_type?: BlackoutType;
  work_order_id?: string | null;
  acknowledge_conflicting_reservations?: boolean;
}

export interface ConflictingReservation {
  id?: string;
  reservation_no?: string;
  requested_by?: string;
  [key: string]: unknown;
}

/** Thrown on 409 when the window overlaps existing reservations and the caller hasn't acknowledged it. */
export class BlackoutConflictError extends Error {
  readonly problem: Problem;
  readonly conflicts: ConflictingReservation[];

  constructor(problem: Problem) {
    super(problem.detail ?? "This window overlaps existing reservations.");
    this.problem = problem;
    // The conflicting-reservations list rides inside `errors[0].message` as a JSON-encoded string.
    const raw = problem.errors?.[0]?.message;
    try {
      const parsed = raw ? JSON.parse(raw) : [];
      this.conflicts = Array.isArray(parsed) ? parsed : [];
    } catch {
      this.conflicts = [];
    }
  }
}

export async function createResourceBlackout(body: CreateBlackoutBody): Promise<{ data?: ResourceBlackout; meta?: { conflicting_reservations?: ConflictingReservation[] } }> {
  try {
    return await apiFetch("/resource-blackouts", { method: "POST", body });
  } catch (err) {
    if (err instanceof ApiError && err.status === 409) throw new BlackoutConflictError(err.problem);
    throw err;
  }
}

export function deleteResourceBlackout(blackoutId: string): Promise<void> {
  return apiFetch(`/resource-blackouts/${blackoutId}`, { method: "DELETE" });
}
