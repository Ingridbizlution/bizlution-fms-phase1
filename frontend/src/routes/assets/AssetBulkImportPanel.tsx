import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { ApiError } from "../../api/client";
import { bulkImportAssets, type BulkImportAssetRow, type BulkImportResult } from "../../api/assets";

const PLACEHOLDER = `asset_code,name,category_code,criticality,status
PUMP-01,1F 排水泵,HVAC.AHU,MEDIUM,ACTIVE
PUMP-02,2F 排水泵,HVAC.AHU,MEDIUM,ACTIVE`;

function parseCsv(text: string, facilityId: string, t: (key: string, opts?: Record<string, unknown>) => string): { rows: BulkImportAssetRow[]; error: string | null } {
  const lines = text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  if (lines.length < 2) return { rows: [], error: t("assets.bulkImportPanel.pasteRowsError") };
  const header = lines[0].split(",").map((h) => h.trim());
  const required = ["asset_code", "name", "category_code"];
  if (!required.every((r) => header.includes(r))) {
    return { rows: [], error: t("assets.bulkImportPanel.headerMustInclude", { fields: required.join(", ") }) };
  }
  const rows = lines.slice(1).map((line) => {
    const cells = line.split(",").map((c) => c.trim());
    const row = Object.fromEntries(header.map((h, i) => [h, cells[i]])) as Record<string, string>;
    return { asset_code: row.asset_code, name: row.name, facility_id: facilityId, category_code: row.category_code, criticality: row.criticality || undefined, status: row.status || undefined };
  });
  return { rows, error: null };
}

function outcomeBadge(outcome: string | undefined): string {
  switch (outcome) {
    case "CREATED":
      return "bg-green-lt";
    case "WOULD_CREATE":
      return "bg-blue-lt";
    case "REJECTED":
      return "bg-red-lt";
    default:
      return "bg-secondary-lt";
  }
}

export function AssetBulkImportPanel({ facilityId, onImported }: { facilityId: string; onImported: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [csv, setCsv] = useState("");
  const [parseError, setParseError] = useState<string | null>(null);
  const [result, setResult] = useState<BulkImportResult | null>(null);
  const [committedRows, setCommittedRows] = useState<BulkImportAssetRow[] | null>(null);

  const mutation = useMutation({
    mutationFn: (dryRun: boolean) => {
      const { rows, error } = parseCsv(csv, facilityId, t);
      if (error) throw new Error(error);
      setCommittedRows(rows);
      return bulkImportAssets(rows, dryRun);
    },
    onSuccess: (res, dryRun) => {
      setParseError(null);
      setResult(res);
      if (!dryRun) {
        queryClient.invalidateQueries({ queryKey: ["assets"] });
        onImported();
      }
    },
    onError: (err) => {
      setResult(null);
      setParseError(err instanceof ApiError ? err.problem.detail ?? err.message : err instanceof Error ? err.message : t("assets.bulkImportPanel.importFailed"));
    },
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      <p className="text-secondary small">{t("assets.bulkImportPanel.hint")}</p>
      <textarea className="form-control mb-2" rows={5} placeholder={PLACEHOLDER} value={csv} onChange={(e) => setCsv(e.target.value)} style={{ fontFamily: "monospace", fontSize: 13 }} />
      {parseError && <div className="alert alert-danger">{parseError}</div>}
      <div className="d-flex gap-2 mb-3">
        <button type="button" className="btn btn-outline-primary" disabled={!csv.trim() || mutation.isPending} onClick={() => mutation.mutate(true)}>
          {t("assets.bulkImportPanel.previewDryRun")}
        </button>
        {result?.dry_run && (
          <button type="button" className="btn btn-primary" disabled={mutation.isPending} onClick={() => mutation.mutate(false)}>
            {t("assets.bulkImportPanel.commitImport", { count: result.accepted, plural: result.accepted === 1 ? "" : "s" })}
          </button>
        )}
      </div>
      {result && (
        <div className="table-responsive">
          <table className="table table-sm">
            <thead>
              <tr>
                <th>{t("assets.bulkImportPanel.colRow")}</th>
                <th>{t("assets.bulkImportPanel.colAssetCode")}</th>
                <th>{t("assets.bulkImportPanel.colOutcome")}</th>
                <th>{t("assets.bulkImportPanel.colError")}</th>
              </tr>
            </thead>
            <tbody>
              {result.rows?.map((r, i) => (
                <tr key={i}>
                  <td>{(committedRows?.[r.index ?? i]?.asset_code ?? r.asset_code) || i + 1}</td>
                  <td>
                    <code>{r.asset_code}</code>
                  </td>
                  <td>
                    <span className={`badge ${outcomeBadge(r.outcome)}`}>{r.outcome}</span>
                  </td>
                  <td className="text-secondary small">{r.error ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="text-secondary small mb-0">
            {t("assets.bulkImportPanel.summary", { accepted: result.accepted, rejected: result.rejected, total: result.total })}
          </p>
        </div>
      )}
    </div>
  );
}
