import { apiFetch, newIdempotencyKey } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type Device = components["schemas"]["Device"];
export type TelemetryLatest = components["schemas"]["TelemetryLatest"];
export type TelemetryBucket = components["schemas"]["TelemetryBucket"];
export type AlarmRule = components["schemas"]["AlarmRule"];
export type Alarm = components["schemas"]["Alarm"];

export function listDevices(facilityId?: string, cursor?: string): Promise<PagedEnvelope<Device>> {
  const query = new URLSearchParams();
  if (facilityId) query.set("facility_id", facilityId);
  if (cursor) query.set("cursor", cursor);
  return apiFetch(`/devices?${query}`);
}

export interface DeviceCreate {
  facility_id: string;
  device_code: string;
  name: string;
  device_type: string;
  spatial_node_id?: string;
  asset_id?: string;
  address?: string;
}

export function createDevice(body: DeviceCreate): Promise<Device> {
  return apiFetch("/devices", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export interface DevicePatch {
  name?: string;
  device_type?: string;
  address?: string;
  offline_alarm_after_seconds?: number;
}

export function updateDevice(deviceId: string, body: DevicePatch): Promise<Device> {
  return apiFetch(`/devices/${deviceId}`, { method: "PATCH", body });
}

export function decommissionDevice(deviceId: string): Promise<{ data?: { id?: string; deleted?: boolean } }> {
  return apiFetch(`/devices/${deviceId}`, { method: "DELETE" });
}

export function getLatestTelemetry(facilityId: string): Promise<{ items?: TelemetryLatest[]; meta?: { stale_count?: number } }> {
  return apiFetch(`/telemetry/latest?facility_id=${facilityId}`);
}

export function getTelemetrySeries(deviceId: string, pointCode: string, from: string, to: string, interval = "5m"): Promise<{ items?: TelemetryBucket[] }> {
  const query = new URLSearchParams({ device_id: deviceId, point_code: pointCode, from, to, interval });
  return apiFetch(`/telemetry/series?${query}`);
}

export function listAlarms(params: { facilityId?: string; status?: string; unlinkedOnly?: boolean; cursor?: string } = {}): Promise<PagedEnvelope<Alarm>> {
  const query = new URLSearchParams();
  if (params.facilityId) query.set("facility_id", params.facilityId);
  if (params.status) query.set("status", params.status);
  if (params.unlinkedOnly) query.set("unlinked_only", "true");
  if (params.cursor) query.set("cursor", params.cursor);
  return apiFetch(`/alarms?${query}`);
}

export function acknowledgeAlarm(alarmId: string, note?: string): Promise<Alarm> {
  return apiFetch(`/alarms/${alarmId}/acknowledge`, { method: "POST", body: { note } });
}

export function suppressAlarm(alarmId: string, durationMinutes: number, reason: string): Promise<{ data: Alarm }> {
  return apiFetch(`/alarms/${alarmId}/suppress`, { method: "POST", body: { duration_minutes: durationMinutes, reason } });
}

export function createWorkOrderFromAlarm(alarmId: string): Promise<{ id?: string; wo_no?: string }> {
  return apiFetch(`/alarms/${alarmId}/work-order`, { method: "POST", idempotencyKey: newIdempotencyKey() });
}

export function listAlarmRules(facilityId?: string): Promise<PagedEnvelope<AlarmRule>> {
  const query = facilityId ? `?facility_id=${facilityId}` : "";
  return apiFetch(`/alarm-rules${query}`);
}

export interface AlarmRuleCreate {
  facility_id?: string;
  code: string;
  name: string;
  point_code?: string;
  condition: { op: string; value: number };
  severity?: string;
  auto_create_work_order?: boolean;
}

export function createAlarmRule(body: AlarmRuleCreate): Promise<AlarmRule> {
  return apiFetch("/alarm-rules", { method: "POST", body });
}

export interface AlarmRulePatch {
  name?: string;
  point_code?: string;
  condition?: { op: string; value: number };
  severity?: string;
  auto_create_work_order?: boolean;
  is_active?: boolean;
}

export function updateAlarmRule(ruleId: string, body: AlarmRulePatch): Promise<AlarmRule> {
  return apiFetch(`/alarm-rules/${ruleId}`, { method: "PATCH", body });
}

export function deleteAlarmRule(ruleId: string): Promise<void> {
  return apiFetch(`/alarm-rules/${ruleId}`, { method: "DELETE" });
}

export function dryRunAlarmRule(alarmRuleId: string): Promise<{ data?: { would_have_fired?: number; sample_triggers?: unknown[] } }> {
  return apiFetch(`/alarm-rules/${alarmRuleId}/test`, { method: "POST" });
}
