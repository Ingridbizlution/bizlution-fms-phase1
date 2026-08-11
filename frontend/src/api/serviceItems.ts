import { apiFetch } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type ServiceItem = components["schemas"]["ServiceItem"];
export type ServiceItemAdmin = components["schemas"]["ServiceItemAdmin"];

export function listServiceItems(facilityId: string): Promise<PagedEnvelope<ServiceItem>> {
  return apiFetch(`/facilities/${facilityId}/service-items`);
}

export interface ServiceItemCreate {
  category: string;
  code: string;
  name: string;
  description?: string;
  default_duration_minutes?: number;
  is_attachable_to_reservation?: boolean;
  is_standalone_requestable?: boolean;
  requires_approval?: boolean;
  chargeable?: boolean;
  unit_price?: number;
}

export function createServiceItem(facilityId: string, body: ServiceItemCreate): Promise<ServiceItem> {
  return apiFetch(`/facilities/${facilityId}/service-items`, { method: "POST", body });
}

export interface ServiceItemPatch {
  category?: string;
  name?: string;
  description?: string | null;
  default_duration_minutes?: number;
  requires_approval?: boolean;
  chargeable?: boolean;
  unit_price?: number | null;
  currency?: string | null;
}

export function updateServiceItem(serviceItemId: string, body: ServiceItemPatch): Promise<ServiceItemAdmin> {
  return apiFetch(`/service-items/${serviceItemId}`, { method: "PATCH", body });
}

export interface DeactivateServiceItemResult {
  data?: { id?: string; deleted?: boolean; open_work_orders?: number };
  meta?: { soft_delete?: boolean; why?: string };
}

export function deactivateServiceItem(serviceItemId: string): Promise<DeactivateServiceItemResult> {
  return apiFetch(`/service-items/${serviceItemId}`, { method: "DELETE" });
}
