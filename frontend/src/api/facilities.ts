import { apiFetch, newIdempotencyKey } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type Facility = components["schemas"]["Facility"];
export type FacilityCreate = components["schemas"]["FacilityCreate"];

export function listFacilities(): Promise<PagedEnvelope<Facility>> {
  return apiFetch("/facilities?limit=100");
}

export function createFacility(body: FacilityCreate): Promise<Facility> {
  return apiFetch("/facilities", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export interface FacilityPatch {
  name?: string;
  facility_type?: string;
  address_line1?: string;
  city?: string;
  country_code?: string;
  timezone?: string;
  gross_area_sqm?: number;
}

export function updateFacility(facilityId: string, body: FacilityPatch): Promise<Facility> {
  return apiFetch(`/facilities/${facilityId}`, { method: "PATCH", body });
}

export function deleteFacility(facilityId: string): Promise<void> {
  return apiFetch(`/facilities/${facilityId}`, { method: "DELETE" });
}
