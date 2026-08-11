import { apiFetch } from "./client";

export type FloorPlanMarkerEntityType = "ASSET" | "DEVICE" | "SPATIAL_NODE";

export interface FloorPlanMarker {
  id: string;
  floor_node_id: string;
  entity_type: FloorPlanMarkerEntityType;
  entity_id: string;
  x_ratio: number;
  y_ratio: number;
  z_offset: number;
  entity_label?: string | null;
  entity_status?: string | null;
  created_at: string;
}

export interface FloorPlanMarkerCreate {
  entity_type: FloorPlanMarkerEntityType;
  entity_id: string;
  x_ratio: number;
  y_ratio: number;
  z_offset?: number;
}

export function listFloorPlanMarkers(floorNodeId: string): Promise<{ items?: FloorPlanMarker[] }> {
  return apiFetch(`/spatial-nodes/${floorNodeId}/floor-plan-markers`);
}

export function createFloorPlanMarker(floorNodeId: string, body: FloorPlanMarkerCreate): Promise<FloorPlanMarker> {
  return apiFetch(`/spatial-nodes/${floorNodeId}/floor-plan-markers`, { method: "POST", body });
}

export function deleteFloorPlanMarker(id: string): Promise<{ deleted?: string }> {
  return apiFetch(`/floor-plan-markers/${id}`, { method: "DELETE" });
}
