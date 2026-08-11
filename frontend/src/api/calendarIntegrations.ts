import { apiFetch } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type CalendarIntegration = components["schemas"]["CalendarIntegration"];

export function listCalendarIntegrations(facilityId: string): Promise<PagedEnvelope<CalendarIntegration>> {
  return apiFetch(`/facilities/${facilityId}/calendar-integrations`);
}

export interface CalendarIntegrationCreate {
  provider: "MS365" | "GOOGLE";
  ms_tenant_id?: string;
}

export function registerCalendarIntegration(facilityId: string, body: CalendarIntegrationCreate): Promise<CalendarIntegration> {
  return apiFetch(`/facilities/${facilityId}/calendar-integrations`, { method: "POST", body });
}

export interface CalendarIntegrationPatch {
  sync_cron?: string;
  status?: "PENDING_CONSENT" | "ACTIVE" | "REVOKED" | "ERROR";
}

export function updateCalendarIntegration(integrationId: string, body: CalendarIntegrationPatch): Promise<CalendarIntegration> {
  return apiFetch(`/calendar-integrations/${integrationId}`, { method: "PATCH", body });
}

export function deleteCalendarIntegration(integrationId: string): Promise<void> {
  return apiFetch(`/calendar-integrations/${integrationId}`, { method: "DELETE" });
}

export interface UnresolvedCalendarResource {
  external_id: string;
  display_name?: string;
}

export function listUnresolvedResources(integrationId: string): Promise<{ data?: UnresolvedCalendarResource[]; meta?: { reason?: string } }> {
  return apiFetch(`/calendar-integrations/${integrationId}/unresolved-resources`);
}

export interface CalendarResourceMapping {
  id: string;
  calendar_integration_id: string;
  spatial_node_id: string;
  node_name?: string | null;
  external_resource_id: string;
  external_resource_name?: string | null;
  sync_direction: "PULL_ONLY" | "PUSH_ONLY" | "BIDIRECTIONAL";
  status: "ACTIVE" | "UNRESOLVED" | "DISABLED";
  created_at: string;
}

export function listCalendarResourceMappings(integrationId: string): Promise<{ data?: CalendarResourceMapping[] }> {
  return apiFetch(`/calendar-integrations/${integrationId}/resource-mappings`);
}

export function createCalendarResourceMapping(integrationId: string, externalResourceId: string, externalResourceName: string | undefined, spatialNodeId: string): Promise<{ data?: { created?: string[] } }> {
  return apiFetch(`/calendar-integrations/${integrationId}/resource-mappings`, {
    method: "POST",
    body: { mappings: [{ external_resource_id: externalResourceId, external_resource_name: externalResourceName, spatial_node_id: spatialNodeId }] },
  });
}

export interface CalendarResourceMappingPatch {
  sync_direction?: "PULL_ONLY" | "PUSH_ONLY" | "BIDIRECTIONAL";
  status?: "ACTIVE" | "UNRESOLVED" | "DISABLED";
}

export function updateCalendarResourceMapping(mappingId: string, body: CalendarResourceMappingPatch): Promise<CalendarResourceMapping> {
  return apiFetch(`/calendar-resource-mappings/${mappingId}`, { method: "PATCH", body });
}

export function deleteCalendarResourceMapping(mappingId: string): Promise<void> {
  return apiFetch(`/calendar-resource-mappings/${mappingId}`, { method: "DELETE" });
}
