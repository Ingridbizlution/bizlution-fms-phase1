import { apiFetch, newIdempotencyKey } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type MaintenanceTemplate = components["schemas"]["MaintenanceTemplate"];
export type MaintenancePlan = components["schemas"]["MaintenancePlan"];
export type MaintenancePlanCreate = components["schemas"]["MaintenancePlanCreate"];
export type MaintenanceOccurrence = components["schemas"]["MaintenanceOccurrence"];

export function listMaintenanceTemplates(): Promise<{ data?: MaintenanceTemplate[] }> {
  return apiFetch("/maintenance-templates");
}

export interface MaintenanceTemplateCreate {
  code: string;
  name: string;
  maintenance_type?: string;
  checklist: { item: string }[];
  estimated_minutes?: number;
}

export function createMaintenanceTemplate(body: MaintenanceTemplateCreate): Promise<MaintenanceTemplate> {
  return apiFetch("/maintenance-templates", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export interface MaintenanceTemplatePatch {
  name?: string;
  maintenance_type?: string;
  checklist?: { item: string }[];
  estimated_minutes?: number;
  is_active?: boolean;
}

export function updateMaintenanceTemplate(templateId: string, body: MaintenanceTemplatePatch): Promise<MaintenanceTemplate> {
  return apiFetch(`/maintenance-templates/${templateId}`, { method: "PATCH", body });
}

export function deleteMaintenanceTemplate(templateId: string): Promise<void> {
  return apiFetch(`/maintenance-templates/${templateId}`, { method: "DELETE" });
}

export function listMaintenancePlans(facilityId?: string, cursor?: string): Promise<PagedEnvelope<MaintenancePlan>> {
  const query = new URLSearchParams();
  if (facilityId) query.set("facility_id", facilityId);
  if (cursor) query.set("cursor", cursor);
  return apiFetch(`/maintenance-plans?${query}`);
}

export function createMaintenancePlan(body: MaintenancePlanCreate): Promise<MaintenancePlan> {
  return apiFetch("/maintenance-plans", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export interface MaintenancePlanPatch {
  name?: string;
  rrule?: string;
  priority?: string;
  generate_lead_days?: number;
  is_active?: boolean;
}

export function updateMaintenancePlan(planId: string, body: MaintenancePlanPatch): Promise<{ data?: MaintenancePlan }> {
  return apiFetch(`/maintenance-plans/${planId}`, { method: "PATCH", body });
}

export function previewMaintenanceSchedule(planId: string, until?: string): Promise<{ data?: { scheduled_for?: string; asset_code?: string }[] }> {
  const query = until ? `?until=${encodeURIComponent(until)}` : "";
  return apiFetch(`/maintenance-plans/${planId}/preview-schedule${query}`);
}

export function generateMaintenanceNow(planId: string): Promise<{ created?: number; skipped?: number; work_order_ids?: string[] }> {
  return apiFetch(`/maintenance-plans/${planId}/generate-now`, { method: "POST" });
}

export function listMaintenanceOccurrences(facilityId?: string, cursor?: string): Promise<PagedEnvelope<MaintenanceOccurrence>> {
  const query = new URLSearchParams();
  if (facilityId) query.set("facility_id", facilityId);
  if (cursor) query.set("cursor", cursor);
  return apiFetch(`/maintenance-occurrences?${query}`);
}

export function skipMaintenanceOccurrence(occurrenceId: string, reason: string): Promise<MaintenanceOccurrence> {
  return apiFetch(`/maintenance-occurrences/${occurrenceId}/skip`, { method: "POST", body: { reason } });
}
