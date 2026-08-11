import { apiFetch } from "./client";
import type { components } from "./schema";

export type BookableResource = components["schemas"]["BookableResource"];

export function listBookableResources(facilityId: string, opts?: { includeUnbookable?: boolean }): Promise<{ data?: BookableResource[] }> {
  const query = new URLSearchParams();
  if (opts?.includeUnbookable) query.set("include_unbookable", "true");
  const qs = query.toString();
  return apiFetch(`/facilities/${facilityId}/bookable-resources${qs ? `?${qs}` : ""}`);
}

/** `resource_type`/`spatial_node_id`/`asset_id`/`facility_id` are immutable — omit them entirely.
 *  `opening_hours`/`attributes` are typed loosely here because the generated schema collapses
 *  an untyped `object` to `Record<string, never>`, which can't hold real values. */
export type BookableResourcePatch = Partial<
  Pick<
    BookableResource,
    | "display_name"
    | "is_bookable"
    | "requires_approval"
    | "requires_check_in"
    | "min_duration_minutes"
    | "max_duration_minutes"
    | "slot_granularity_minutes"
    | "buffer_before_minutes"
    | "buffer_after_minutes"
    | "advance_booking_days"
    | "min_notice_minutes"
    | "capacity"
    | "auto_release_minutes"
    | "approver_role_code"
    | "max_active_per_user"
  >
> & {
  opening_hours?: Record<string, [string, string][]>;
  attributes?: Record<string, unknown>;
};

export function updateBookableResource(resourceId: string, patch: BookableResourcePatch): Promise<BookableResource> {
  return apiFetch<BookableResource>(`/bookable-resources/${resourceId}`, { method: "PATCH", body: patch });
}
