import { apiFetch } from "./client";
import type { components } from "./schema";

export type FacilityDashboard = components["schemas"]["FacilityDashboard"];
export type ReportExport = components["schemas"]["ReportExport"];
export type GroupRollupRow = components["schemas"]["GroupRollupRow"];
export type AssetReliabilityRow = components["schemas"]["AssetReliabilityRow"];
export type SpaceUtilizationRow = components["schemas"]["SpaceUtilizationRow"];
export type ServiceVolumeRow = components["schemas"]["ServiceVolumeRow"];
export type PmComplianceRow = components["schemas"]["PmComplianceRow"];

export interface SlaComplianceRow {
  group_key: string | null;
  group_label: string;
  response_total: number;
  response_met: number;
  response_breached: number;
  response_compliance_pct?: number | null;
  avg_response_minutes?: number | null;
  resolution_total: number;
  resolution_met: number;
  resolution_breached: number;
  resolution_compliance_pct?: number | null;
  avg_resolution_minutes?: number | null;
}

export function getFacilityDashboard(facilityId: string, period: "today" | "7d" | "30d" | "mtd" | "qtd" = "7d"): Promise<FacilityDashboard> {
  const query = new URLSearchParams({ facility_id: facilityId, period });
  return apiFetch<FacilityDashboard>(`/reports/facility-dashboard?${query}`);
}

export interface DateRange {
  from: string;
  to: string;
}

export function getSlaCompliance(range: DateRange & { groupBy?: string }): Promise<{ data: SlaComplianceRow[]; meta: Record<string, unknown> }> {
  const query = new URLSearchParams({ from: range.from, to: range.to, group_by: range.groupBy ?? "facility" });
  return apiFetch(`/reports/sla-compliance?${query}`);
}

export function getPmComplianceReport(range: DateRange & { groupBy?: string }): Promise<{ data?: PmComplianceRow[]; meta?: Record<string, unknown> }> {
  const query = new URLSearchParams({ from: range.from, to: range.to, group_by: range.groupBy ?? "facility" });
  return apiFetch(`/reports/pm-compliance?${query}`);
}

export function getGroupRollupReport(range: DateRange): Promise<{ data?: GroupRollupRow[]; meta?: Record<string, unknown> }> {
  const query = new URLSearchParams({ from: range.from, to: range.to });
  return apiFetch(`/reports/group-rollup?${query}`);
}

export function getAssetReliabilityReport(range: DateRange & { facilityId?: string }): Promise<{ data?: AssetReliabilityRow[]; meta?: Record<string, unknown> }> {
  const query = new URLSearchParams({ from: range.from, to: range.to });
  if (range.facilityId) query.set("facility_id", range.facilityId);
  return apiFetch(`/reports/asset-reliability?${query}`);
}

export function getSpaceUtilizationReport(range: DateRange & { facilityId?: string }): Promise<{ data?: SpaceUtilizationRow[]; meta?: Record<string, unknown> }> {
  const query = new URLSearchParams({ from: range.from, to: range.to });
  if (range.facilityId) query.set("facility_id", range.facilityId);
  return apiFetch(`/reports/space-utilization?${query}`);
}

export function getServiceVolumeReport(range: DateRange & { groupBy?: string }): Promise<{ data?: ServiceVolumeRow[]; meta?: Record<string, unknown> }> {
  const query = new URLSearchParams({ from: range.from, to: range.to, group_by: range.groupBy ?? "service_item" });
  return apiFetch(`/reports/service-volume?${query}`);
}

export type ReportCode = "sla-compliance" | "pm-compliance" | "group-rollup" | "asset-reliability" | "space-utilization" | "service-volume";

export function exportReport(
  code: ReportCode,
  params: DateRange & { format?: "csv" | "xlsx"; groupBy?: string; facilityId?: string },
): Promise<ReportExport> {
  // The export endpoint 422s on unrecognized body keys, so camelCase filters
  // from the tabs must be translated to the snake_case names it accepts —
  // never forwarded as extra keys alongside them.
  const body: Record<string, unknown> = { from: params.from, to: params.to, format: params.format ?? "csv" };
  if (params.groupBy) body.group_by = params.groupBy;
  if (params.facilityId) body.facility_id = params.facilityId;
  return apiFetch(`/reports/${code}:export`, { method: "POST", body });
}

export function getReportExport(id: string): Promise<ReportExport> {
  return apiFetch(`/reports/exports/${id}`);
}
