import { apiFetch } from "./client";
import type { components } from "./schema";

export type Tenant = components["schemas"]["Tenant"];

/**
 * `GET /tenant` 的 `meta.read_only_fields`：後端逐一列出改不了的欄位與理由。
 * 前端刻意**不**自己維護一份唯讀清單 —— 那份清單會與後端分歧，而症狀是一個
 * 永遠回 422 的表單。
 */
export interface TenantReadOnlyField {
  field: string;
  reason: string;
}

export interface TenantEnvelope {
  data: Tenant;
  meta?: { read_only_fields?: TenantReadOnlyField[] };
}

export function getTenant(): Promise<TenantEnvelope> {
  return apiFetch("/tenant");
}

/**
 * `PATCH /tenant` 只收租戶自己擁有的六個欄位；合約與授權那組送了會整個 422。
 *
 * `legal_name` 送 `null` 是**清空**，不送則是不動 —— 後端用 `Option<Option<_>>`
 * 區分這兩件事，所以這裡的型別要能表達 `null`。
 */
export interface TenantPatch {
  name?: string;
  legal_name?: string | null;
  default_timezone?: string;
  default_locale?: string;
  default_currency?: string;
  settings?: Record<string, unknown>;
}

export function updateTenant(body: TenantPatch): Promise<{ data: Tenant }> {
  return apiFetch("/tenant", { method: "PATCH", body });
}
