import { apiFetch } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type Organization = components["schemas"]["Organization"];
export type OrganizationCreate = components["schemas"]["OrganizationCreate"];

export function listOrganizations(): Promise<PagedEnvelope<Organization>> {
  return apiFetch("/organizations?limit=100");
}

export function createOrganization(body: OrganizationCreate): Promise<Organization> {
  return apiFetch("/organizations", { method: "POST", body });
}

export interface OrganizationPatch {
  code?: string;
  name?: string;
  org_type?: string;
  cost_center?: string | null;
  status?: string;
}

export function updateOrganization(orgId: string, body: OrganizationPatch): Promise<Organization> {
  return apiFetch(`/organizations/${orgId}`, { method: "PATCH", body });
}

export interface DeleteOrganizationResult {
  data?: { id?: string; deleted?: boolean };
  meta?: { soft_delete?: boolean; users_still_referencing?: number };
}

export function deleteOrganization(orgId: string): Promise<DeleteOrganizationResult> {
  return apiFetch(`/organizations/${orgId}`, { method: "DELETE" });
}
