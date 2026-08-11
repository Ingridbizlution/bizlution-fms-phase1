import { apiFetch, newIdempotencyKey } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type WorkOrder = components["schemas"]["WorkOrder"];
export type WorkOrderDetail = components["schemas"]["WorkOrderDetail"];
export type WorkOrderCreate = components["schemas"]["WorkOrderCreate"];
export type WorkOrderUpdate = components["schemas"]["WorkOrderUpdate"];
export type WorkOrderStatusDict = { code?: string; name_zh?: string; name_en?: string; category?: string; is_terminal?: boolean };
export interface AvailableAction {
  action?: string;
  to_status?: string;
  label_zh?: string;
  required_fields?: string[];
  permitted?: boolean;
}

export interface ListWorkOrdersParams {
  facilityId?: string;
  statusCategory?: string;
  status?: string;
  priority?: string;
  mine?: boolean;
  cursor?: string;
  limit?: number;
}

export function listWorkOrders(params: ListWorkOrdersParams): Promise<PagedEnvelope<WorkOrder>> {
  const query = new URLSearchParams();
  if (params.facilityId) query.set("facility_id", params.facilityId);
  if (params.statusCategory) query.set("status_category", params.statusCategory);
  if (params.status) query.set("status", params.status);
  if (params.priority) query.set("priority", params.priority);
  if (params.mine) query.set("mine", "true");
  if (params.cursor) query.set("cursor", params.cursor);
  query.set("limit", String(params.limit ?? 50));
  return apiFetch<PagedEnvelope<WorkOrder>>(`/work-orders?${query}`);
}

export function getWorkOrder(workOrderId: string, include?: string): Promise<WorkOrderDetail> {
  const query = include ? `?include=${encodeURIComponent(include)}` : "";
  return apiFetch<WorkOrderDetail>(`/work-orders/${workOrderId}${query}`);
}

export function createWorkOrder(body: WorkOrderCreate): Promise<WorkOrder> {
  return apiFetch<WorkOrder>("/work-orders", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export function listAvailableActions(workOrderId: string): Promise<{ data?: AvailableAction[] }> {
  return apiFetch(`/work-orders/${workOrderId}/available-actions`);
}

export function updateWorkOrder(workOrderId: string, version: number, body: WorkOrderUpdate): Promise<WorkOrder> {
  return apiFetch<WorkOrder>(`/work-orders/${workOrderId}`, { method: "PATCH", body, ifMatch: version });
}

export function transitionWorkOrder(
  workOrderId: string,
  version: number,
  body: { action: string; assignee_id?: string; team_id?: string; reason?: string; resolution_notes?: string },
): Promise<WorkOrder> {
  return apiFetch<WorkOrder>(`/work-orders/${workOrderId}/transitions`, {
    method: "POST",
    body,
    ifMatch: version,
    idempotencyKey: newIdempotencyKey(),
  });
}

export function addWorkOrderComment(workOrderId: string, body: string, visibility: "PUBLIC" | "INTERNAL" = "INTERNAL") {
  return apiFetch(`/work-orders/${workOrderId}/comments`, { method: "POST", body: { body, visibility } });
}

export function updateWorkOrderTask(workOrderId: string, taskId: string, body: { result_value?: unknown; is_pass?: boolean; notes?: string }) {
  return apiFetch(`/work-orders/${workOrderId}/tasks/${taskId}`, { method: "PATCH", body });
}

export function listWorkOrderStatuses(): Promise<{ data?: WorkOrderStatusDict[] }> {
  return apiFetch("/work-order-statuses");
}
