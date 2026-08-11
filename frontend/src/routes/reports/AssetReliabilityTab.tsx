import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { getAssetReliabilityReport } from "../../api/reports";
import { useAuth } from "../../auth/AuthContext";
import { priorityBadge } from "../../lib/statusColors";
import { BarChart } from "../../shell/charts/BarChart";
import { TABLER_COLORS } from "../../shell/charts/chartTheme";
import { DateRangeFilter, defaultRange } from "../../shell/DateRangeFilter";
import { EmptyState } from "../../shell/EmptyState";
import { ExportButton } from "../../shell/ExportButton";

export function AssetReliabilityTab({ onExportQueued }: { onExportQueued: (code: string, id: string) => void }) {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [range, setRange] = useState(defaultRange(180));

  const { data, isLoading, isError } = useQuery({
    queryKey: ["report-asset-reliability", range, facilityId],
    queryFn: () => getAssetReliabilityReport({ ...range, facilityId: facilityId ?? undefined }),
  });

  return (
    <div className="card">
      <div className="card-header flex-wrap gap-2">
        <DateRangeFilter value={range} onChange={setRange} />
        <ExportButton code="asset-reliability" range={range} facilityId={facilityId ?? undefined} onQueued={(id) => onExportQueued(t("reports.tabReliability"), id)} />
      </div>
      {isLoading && (
        <div className="d-flex justify-content-center py-5">
          <div className="spinner-border text-primary" role="status" aria-label={t("reports.loadingReport")} />
        </div>
      )}
      {isError && <div className="alert alert-danger m-3">{t("reports.loadReportError")}</div>}
      {data && data.data && data.data.length > 0 && (
        <div className="card-body border-bottom">
          <BarChart
            categories={data.data.map((row) => row.asset_code ?? "—")}
            series={[{ name: t("reports.colFailures"), data: data.data.map((row) => row.failure_count ?? 0), color: TABLER_COLORS.orange }]}
            horizontal
          />
        </div>
      )}
      {data && (
        <div className="table-responsive">
          <table className="table table-vcenter card-table">
            <thead>
              <tr>
                <th>{t("reports.colAsset")}</th>
                <th>{t("reports.colCriticality")}</th>
                <th>{t("reports.colFailures")}</th>
                <th>{t("reports.colMtbf")}</th>
                <th>{t("reports.colMttr")}</th>
                <th>{t("reports.colDowntime")}</th>
                <th>{t("reports.colRepairCost")}</th>
              </tr>
            </thead>
            <tbody>
              {data.data?.map((row) => (
                <tr key={row.asset_id}>
                  <td>
                    <code>{row.asset_code}</code>
                    <div>{row.asset_name}</div>
                  </td>
                  <td>
                    <span className={`badge ${priorityBadge(row.criticality)}`}>{row.criticality}</span>
                  </td>
                  <td>{row.failure_count}</td>
                  <td>{row.mtbf_hours != null ? t("reports.hoursShort", { count: Math.round(row.mtbf_hours) }) : <span className="text-secondary">{t("reports.notAvailable")}</span>}</td>
                  <td>{row.mttr_hours != null ? `${row.mttr_hours.toFixed(1)}h` : "—"}</td>
                  <td>{row.downtime_hours?.toFixed(1)}h</td>
                  <td className="text-secondary">{row.repair_cost?.toLocaleString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {data.data?.length === 0 && <EmptyState title={t("reports.noAssetsInScope")} />}
        </div>
      )}
    </div>
  );
}
