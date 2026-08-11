import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { getSpaceUtilizationReport } from "../../api/reports";
import { useAuth } from "../../auth/AuthContext";
import { BarChart } from "../../shell/charts/BarChart";
import { TABLER_COLORS } from "../../shell/charts/chartTheme";
import { DateRangeFilter, defaultRange } from "../../shell/DateRangeFilter";
import { EmptyState } from "../../shell/EmptyState";
import { ExportButton } from "../../shell/ExportButton";
import { PercentBar } from "../../shell/PercentBar";

export function SpaceUtilizationTab({ onExportQueued }: { onExportQueued: (code: string, id: string) => void }) {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [range, setRange] = useState(defaultRange());

  const { data, isLoading, isError } = useQuery({
    queryKey: ["report-space-utilization", range, facilityId],
    queryFn: () => getSpaceUtilizationReport({ ...range, facilityId: facilityId ?? undefined }),
  });

  return (
    <div className="card">
      <div className="card-header flex-wrap gap-2">
        <DateRangeFilter value={range} onChange={setRange} />
        <ExportButton code="space-utilization" range={range} facilityId={facilityId ?? undefined} onQueued={(id) => onExportQueued(t("reports.tabSpace"), id)} />
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
            categories={data.data.map((row) => row.resource_name ?? "—")}
            series={[{ name: t("reports.colUtilization"), data: data.data.map((row) => (row.utilization_rate != null ? row.utilization_rate * 100 : 0)), color: TABLER_COLORS.teal }]}
            valueSuffix="%"
          />
        </div>
      )}
      {data && (
        <div className="table-responsive">
          <table className="table table-vcenter card-table">
            <thead>
              <tr>
                <th>{t("reports.colResource")}</th>
                <th>{t("reports.colReservations")}</th>
                <th>{t("reports.colBookedHours")}</th>
                <th>{t("reports.colUtilization")}</th>
                <th>{t("reports.colNoShowRate")}</th>
                <th>{t("reports.colCancelled")}</th>
              </tr>
            </thead>
            <tbody>
              {data.data?.map((row) => (
                <tr key={row.resource_id}>
                  <td>
                    {row.resource_name}
                    {row.hours_basis === "assumed_24h" && <span className="badge bg-yellow-lt ms-1">{t("reports.assumedHrs")}</span>}
                  </td>
                  <td>{row.reservations_total}</td>
                  <td>
                    {row.booked_hours?.toFixed(1)} / {row.available_hours?.toFixed(0)}
                  </td>
                  <td>
                    <PercentBar value={row.utilization_rate != null ? row.utilization_rate * 100 : null} />
                  </td>
                  <td>{row.no_show_rate != null ? `${(row.no_show_rate * 100).toFixed(1)}%` : <span className="text-secondary">{t("reports.notAvailable")}</span>}</td>
                  <td className="text-secondary">{row.cancelled}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {data.data?.length === 0 && <EmptyState title={t("reports.noBookableResourcesInScope")} />}
        </div>
      )}
    </div>
  );
}
