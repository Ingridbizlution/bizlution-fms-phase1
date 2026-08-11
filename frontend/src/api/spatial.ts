import { apiFetch, newIdempotencyKey } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type SpatialNode = components["schemas"]["SpatialNode"];
export type SpatialNodeCreate = components["schemas"]["SpatialNodeCreate"];
export type BimModel = components["schemas"]["BimModel"];
export type BimModelCreate = components["schemas"]["BimModelCreate"];
export type FloorViewNode = components["schemas"]["FloorViewNode"];
export type UnresolvedBimElement = components["schemas"]["UnresolvedBimElement"];

export function listSpatialNodes(facilityId: string, params: { floorLevel?: number; cursor?: string } = {}): Promise<PagedEnvelope<SpatialNode>> {
  const query = new URLSearchParams({ view: "flat", include_asset_counts: "true" });
  if (params.floorLevel != null) query.set("floor_level", String(params.floorLevel));
  if (params.cursor) query.set("cursor", params.cursor);
  return apiFetch(`/facilities/${facilityId}/spatial-nodes?${query}`);
}

export function createSpatialNode(facilityId: string, body: SpatialNodeCreate): Promise<SpatialNode> {
  return apiFetch(`/facilities/${facilityId}/spatial-nodes`, { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export interface SpatialNodePatch {
  code?: string;
  name?: string;
  node_type_code?: string;
  capacity?: number;
  is_bookable?: boolean;
  is_active?: boolean;
  parent_id?: string | null;
  floor_level?: number | null;
  floor_label?: string | null;
  area_sqm?: number | null;
  bim_element_id?: string | null;
}

export function updateSpatialNode(nodeId: string, body: SpatialNodePatch): Promise<SpatialNode> {
  return apiFetch(`/spatial-nodes/${nodeId}`, { method: "PATCH", body });
}

export interface DeleteSpatialNodeResult {
  data?: { id?: string; deleted?: boolean };
  meta?: { soft_delete?: boolean; assets_still_referencing?: number; maintenance_plans_still_referencing?: number; why_soft?: string };
}

export function deleteSpatialNode(nodeId: string): Promise<DeleteSpatialNodeResult> {
  return apiFetch(`/spatial-nodes/${nodeId}`, { method: "DELETE" });
}

export function listSpatialNodeTypes(): Promise<{ data?: { code?: string; name?: string; level_hint?: number; is_bookable?: boolean }[] }> {
  return apiFetch("/spatial-node-types");
}

export function listBimModels(facilityId: string): Promise<PagedEnvelope<BimModel>> {
  return apiFetch(`/facilities/${facilityId}/bim-models`);
}

export function registerBimModel(facilityId: string, body: BimModelCreate): Promise<BimModel> {
  return apiFetch(`/facilities/${facilityId}/bim-models`, { method: "POST", body });
}

export function getBimModel(bimModelId: string): Promise<{ data?: BimModel & { parse_report?: unknown }; meta?: { awaiting_parse?: boolean } }> {
  return apiFetch(`/bim-models/${bimModelId}`);
}

export function deleteBimModel(bimModelId: string): Promise<void> {
  return apiFetch(`/bim-models/${bimModelId}`, { method: "DELETE" });
}

export function resetBimModel(bimModelId: string): Promise<BimModel> {
  return apiFetch(`/bim-models/${bimModelId}/reset`, { method: "POST" });
}

export function presignUpload(fileName: string, contentType: string, contentLength: number): Promise<{ upload_url?: string; storage_key?: string; content_type?: string }> {
  return apiFetch("/uploads/presign", { method: "POST", body: { file_name: fileName, content_type: contentType, content_length: contentLength, purpose: "BIM_MODEL" } });
}

export function getFloorView(facilityId: string, floorLevel?: number): Promise<{ data?: FloorViewNode[]; meta?: { floors?: number[]; node_count?: number } }> {
  const query = floorLevel != null ? `?floor_level=${floorLevel}` : "";
  return apiFetch(`/facilities/${facilityId}/floor-view${query}`);
}

export function listUnresolvedBimElements(bimModelId: string): Promise<{ data?: UnresolvedBimElement[] }> {
  return apiFetch(`/bim-models/${bimModelId}/unresolved-elements`);
}

export interface BimMappingResult {
  bim_element_id?: string;
  target_type?: string;
  target_id?: string;
  ok?: boolean;
  error?: string;
}

export function createBimMappings(
  bimModelId: string,
  mappings: { bim_element_id: string; target_type: "SPATIAL_NODE" | "ASSET"; target_id: string }[],
): Promise<{ data?: BimMappingResult[]; meta?: { applied?: number; rejected?: number; unresolved_count?: number } }> {
  return apiFetch(`/bim-models/${bimModelId}/mappings`, { method: "POST", body: { mappings } });
}
