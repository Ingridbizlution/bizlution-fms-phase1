import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { getTenant, updateTenant, type Tenant, type TenantPatch } from "../../api/tenant";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";

/**
 * 租戶設定（`GET`／`PATCH /tenant`）。
 *
 * # 為什麼分成兩張表
 *
 * 後端把欄位分成兩組：租戶自己擁有的（可改）與合約決定的（唯讀）。
 * `plan_tier` 與 `quota_*` 看起來就是「設定」，把它們跟名稱排在同一張
 * 表單裡，使用者會去改它們然後拿到 422。分開陳列讓「這是你買的東西」
 * 與「這是你可以調的東西」在版面上就是兩件事。
 *
 * 唯讀那組的理由直接用 `meta.read_only_fields` 顯示，不在前端複製一份。
 */
export function TenantTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);

  const query = useQuery({ queryKey: ["tenant"], queryFn: getTenant });
  const tenant = query.data?.data;
  const readOnly = query.data?.meta?.read_only_fields ?? [];

  if (query.isLoading) {
    return (
      <div className="card">
        <div className="card-body text-center">
          <div className="spinner-border text-primary" role="status" aria-label={t("common.loading")} />
        </div>
      </div>
    );
  }

  if (query.isError) {
    const err = query.error;
    // `tenant:read` 是 TENANT 範圍：場域級管理員沒有它。講清楚是權限不足，
    // 而不是丟一個裸的 403 —— 後者看起來像壞掉。
    const denied = err instanceof ApiError && err.problem.status === 403;
    return (
      <div className="card">
        <div className="card-body">
          <div className={`alert ${denied ? "alert-warning" : "alert-danger"} mb-0`}>
            {denied ? t("admin.tenantForbidden") : err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.tenantLoadError")}
          </div>
        </div>
      </div>
    );
  }

  if (!tenant) return null;

  return (
    <>
      <div className="card mb-3">
        <div className="card-header">
          <h3 className="card-title">{t("admin.tenantTitle")}</h3>
          <Can permission="tenant:update">
            <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setEditing((s) => !s)}>
              {editing ? t("common.cancel") : t("common.edit")}
            </button>
          </Can>
        </div>

        {editing ? (
          <TenantForm
            tenant={tenant}
            onDone={() => {
              setEditing(false);
              void queryClient.invalidateQueries({ queryKey: ["tenant"] });
            }}
          />
        ) : (
          <div className="table-responsive">
            <table className="table table-vcenter card-table">
              <tbody>
                <Row label={t("admin.tenantCode")}>
                  <code>{tenant.code}</code>
                </Row>
                <Row label={t("common.name")}>{tenant.name}</Row>
                <Row label={t("admin.legalName")}>{tenant.legal_name ?? "—"}</Row>
                <Row label={t("admin.industry")}>{tenant.industry}</Row>
                <Row label={t("admin.defaultTimezone")}>{tenant.default_timezone}</Row>
                <Row label={t("admin.defaultLocale")}>{tenant.default_locale}</Row>
                <Row label={t("admin.defaultCurrency")}>{tenant.default_currency}</Row>
                <Row label={t("admin.tenantSettings")}>
                  <pre className="mb-0 small">{JSON.stringify(tenant.settings ?? {}, null, 2)}</pre>
                </Row>
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div className="card">
        <div className="card-header">
          <h3 className="card-title">{t("admin.contractTitle")}</h3>
          <span className="card-subtitle ms-auto text-secondary small">{t("admin.contractReadOnly")}</span>
        </div>
        <div className="table-responsive">
          <table className="table table-vcenter card-table">
            <tbody>
              <Row label={t("admin.planTier")}>
                <span className="badge bg-blue-lt">{tenant.plan_tier}</span>
              </Row>
              <Row label={t("common.status")}>
                <span className={`badge ${tenant.status === "ACTIVE" ? "bg-green-lt" : "bg-secondary-lt"}`}>{tenant.status}</span>
              </Row>
              <Row label={t("admin.isolationMode")}>{tenant.isolation_mode}</Row>
              <Row label={t("admin.contractPeriod")}>
                {tenant.contract_start_date || tenant.contract_end_date ? `${tenant.contract_start_date ?? "—"} → ${tenant.contract_end_date ?? "—"}` : "—"}
              </Row>
              <Row label={t("admin.quotaApiRps")}>{tenant.quota_api_rps ?? "—"}</Row>
              <Row label={t("admin.quotaAssets")}>{tenant.quota_assets ?? t("admin.quotaUnlimited")}</Row>
              <Row label={t("admin.quotaUsers")}>{tenant.quota_users ?? t("admin.quotaUnlimited")}</Row>
              <Row label={t("admin.featureFlags")}>
                <FeatureFlags flags={tenant.feature_flags} />
              </Row>
            </tbody>
          </table>
        </div>
        {readOnly.length > 0 && (
          <div className="card-body border-top">
            <div className="text-secondary small mb-2">{t("admin.readOnlyFieldsHint")}</div>
            <ul className="list-unstyled mb-0 small">
              {readOnly.map((f) => (
                <li key={f.field} className="mb-1">
                  <code className="me-2">{f.field}</code>
                  <span className="text-secondary">{f.reason}</span>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <tr>
      <td className="text-secondary" style={{ width: "16rem" }}>
        {label}
      </td>
      <td>{children}</td>
    </tr>
  );
}

function FeatureFlags({ flags }: { flags: Tenant["feature_flags"] }) {
  const entries = Object.entries((flags ?? {}) as Record<string, unknown>);
  if (entries.length === 0) return <>—</>;
  return (
    <div className="d-flex flex-wrap gap-1">
      {entries.map(([key, value]) => (
        <span className={`badge ${value ? "bg-green-lt" : "bg-secondary-lt"}`} key={key}>
          {key}
        </span>
      ))}
    </div>
  );
}

function TenantForm({ tenant, onDone }: { tenant: Tenant; onDone: () => void }) {
  const { t } = useTranslation();
  const [name, setName] = useState(tenant.name ?? "");
  const [legalName, setLegalName] = useState(tenant.legal_name ?? "");
  const [timezone, setTimezone] = useState(tenant.default_timezone ?? "");
  const [locale, setLocale] = useState(tenant.default_locale ?? "");
  const [currency, setCurrency] = useState(tenant.default_currency ?? "");
  const [settingsText, setSettingsText] = useState(() => JSON.stringify(tenant.settings ?? {}, null, 2));
  const [jsonError, setJsonError] = useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: () => {
      const body: TenantPatch = {
        name,
        // 空字串是**清空**（送 null），不是「不動」—— 後端用 Option<Option<_>>
        // 區分這兩件事，前端這裡是唯一能表達「清空」的地方。
        legal_name: legalName.trim() === "" ? null : legalName,
        default_timezone: timezone,
        default_locale: locale,
        default_currency: currency,
        settings: JSON.parse(settingsText) as Record<string, unknown>,
      };
      return updateTenant(body);
    },
    onSuccess: onDone,
  });

  function save() {
    try {
      const parsed: unknown = JSON.parse(settingsText);
      if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
        setJsonError(t("admin.settingsMustBeObject"));
        return;
      }
    } catch {
      // 先在本地擋掉語法錯誤：送出去的話後端回的是一個關於 JSON body 的
      // 錯誤，那離「你第 3 行少了一個逗號」很遠。
      setJsonError(t("admin.settingsInvalidJson"));
      return;
    }
    setJsonError(null);
    mutation.mutate();
  }

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? (mutation.error.problem.detail ?? mutation.error.message) : t("admin.saveTenantError")}</div>
      )}
      <div className="row g-2">
        <div className="col-md-4">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-4">
          <label className="form-label">{t("admin.legalName")}</label>
          <input className="form-control" value={legalName} onChange={(e) => setLegalName(e.target.value)} placeholder={t("admin.legalNameClearHint")} />
        </div>
        <div className="col-md-4">
          <label className="form-label">{t("admin.defaultTimezone")}</label>
          <input className="form-control" value={timezone} onChange={(e) => setTimezone(e.target.value)} placeholder="Asia/Taipei" />
        </div>
        <div className="col-md-4">
          <label className="form-label">{t("admin.defaultLocale")}</label>
          <input className="form-control" value={locale} onChange={(e) => setLocale(e.target.value)} placeholder="zh-TW" />
        </div>
        <div className="col-md-4">
          <label className="form-label">{t("admin.defaultCurrency")}</label>
          <input className="form-control" value={currency} onChange={(e) => setCurrency(e.target.value)} placeholder="TWD" />
        </div>
        <div className="col-12">
          <label className="form-label">{t("admin.tenantSettings")}</label>
          <textarea
            className={`form-control font-monospace ${jsonError ? "is-invalid" : ""}`}
            rows={6}
            value={settingsText}
            onChange={(e) => {
              setSettingsText(e.target.value);
              setJsonError(null);
            }}
          />
          {jsonError && <div className="invalid-feedback d-block">{jsonError}</div>}
          <div className="form-hint">{t("admin.settingsHint")}</div>
        </div>
        <div className="col-12 d-flex gap-2">
          <button type="button" className="btn btn-primary" disabled={mutation.isPending || !name} onClick={save}>
            {mutation.isPending ? t("common.saving") : t("common.save")}
          </button>
          <button type="button" className="btn btn-outline-secondary" onClick={onDone}>
            {t("common.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}
