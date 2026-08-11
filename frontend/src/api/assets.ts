import { apiFetch, newIdempotencyKey } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type Asset = components["schemas"]["Asset"];
export type AssetDetail = components["schemas"]["AssetDetail"];
export type AssetCreate = components["schemas"]["AssetCreate"];
export type AssetCategory = components["schemas"]["AssetCategory"];

export interface ListAssetsParams {
  facilityId?: string;
  categoryCode?: string;
  status?: string;
  q?: string;
  cursor?: string;
  limit?: number;
}

export function listAssets(params: ListAssetsParams): Promise<PagedEnvelope<Asset>> {
  const query = new URLSearchParams();
  if (params.facilityId) query.set("facility_id", params.facilityId);
  if (params.categoryCode) query.set("category_code", params.categoryCode);
  if (params.status) query.set("status", params.status);
  if (params.q) query.set("q", params.q);
  if (params.cursor) query.set("cursor", params.cursor);
  query.set("limit", String(params.limit ?? 50));
  return apiFetch<PagedEnvelope<Asset>>(`/assets?${query}`);
}

export function getAsset(assetId: string, include?: string): Promise<AssetDetail> {
  const query = include ? `?include=${encodeURIComponent(include)}` : "";
  return apiFetch<AssetDetail>(`/assets/${assetId}${query}`);
}

export function createAsset(body: AssetCreate): Promise<Asset> {
  return apiFetch<Asset>("/assets", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export function listAssetCategories(): Promise<{ data?: AssetCategory[] }> {
  return apiFetch("/asset-categories");
}

export type AssetModel = components["schemas"]["AssetModel"] & { is_active?: boolean };

export interface ListAssetModelsParams {
  categoryCode?: string;
  manufacturer?: string;
  isActive?: boolean;
  limit?: number;
}

export function listAssetModels(params: ListAssetModelsParams = {}): Promise<PagedEnvelope<AssetModel>> {
  const query = new URLSearchParams();
  if (params.categoryCode) query.set("category_code", params.categoryCode);
  if (params.manufacturer) query.set("manufacturer", params.manufacturer);
  if (params.isActive !== undefined) query.set("is_active", String(params.isActive));
  query.set("limit", String(params.limit ?? 100));
  return apiFetch<PagedEnvelope<AssetModel>>(`/asset-models?${query}`);
}

export interface AssetModelCreate {
  category_id: string;
  manufacturer: string;
  model_no: string;
  name: string;
  supported_protocols?: string[];
  expected_life_months?: number;
}

export function createAssetModel(body: AssetModelCreate): Promise<AssetModel> {
  return apiFetch("/asset-models", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export interface AssetModelPatch {
  name?: string;
  supported_protocols?: string[];
  expected_life_months?: number;
  is_active?: boolean;
}

export function updateAssetModel(modelId: string, body: AssetModelPatch): Promise<AssetModel> {
  return apiFetch(`/asset-models/${modelId}`, { method: "PATCH", body });
}

export function deleteAssetModel(modelId: string): Promise<void> {
  return apiFetch(`/asset-models/${modelId}`, { method: "DELETE" });
}

export interface BulkImportAssetRow {
  asset_code: string;
  name: string;
  facility_id: string;
  category_code: string;
  status?: string;
  criticality?: string;
}

export interface BulkImportResult {
  dry_run?: boolean;
  total?: number;
  accepted?: number;
  rejected?: number;
  rows?: {
    index?: number;
    asset_code?: string | null;
    outcome?: "CREATED" | "WOULD_CREATE" | "REJECTED";
    asset_id?: string | null;
    error_code?: string | null;
    error?: string | null;
  }[];
}

export function bulkImportAssets(rows: BulkImportAssetRow[], dryRun: boolean): Promise<BulkImportResult> {
  return apiFetch("/assets:bulk-import", { method: "POST", body: { dry_run: dryRun, rows } });
}
