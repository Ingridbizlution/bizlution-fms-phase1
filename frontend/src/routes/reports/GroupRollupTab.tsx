import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { getGroupRollupReport } from "../../api/reports";
import { BarChart } from "../../shell/charts/BarChart";
import { TABLER_COLORS } from "../../shell/charts/chartTheme";
import { DateRangeFilter, defaultRange } from "../../shell/DateRangeFilter";
import { EmptyState } from "../../shell/EmptyState";
import { ExportButton } from "../../shell/ExportButton";
import { PercentBar } from "../../shell/PercentBar";

export function GroupRollupTab({ onExportQueued }: { onExportQueued: (code: string, id: string) => void }) {
  const { t } = useTranslation();
  const [range, setRange] = useState(defaultRange(90));

  const { data, isLoading, isError } = useQuery({
    queryKey: ["report-group-rollup", range],
    queryFn: () => getGroupRollupReport(range),
  });

  return (
    <div className="card">
      <div className="card-header flex-wrap gap-2">
        <DateRangeFilter value={range} onChange={setRange} />
        <ExportButton code="group-rollup" range={range} onQueued={(id) => onExportQueued(t("reports.tabRollup"), id)} />
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
            categories={data.data.map((row) => row.org_name ?? "—")}
            series={[
              { name: t("reports.workOrdersOpenSeries"), data: data.data.map((row) => row.work_orders_open ?? 0), color: TABLER_COLORS.azure },
              { name: t("reports.workOrdersOverdueSeries"), data: data.data.map((row) => row.work_orders_overdue ?? 0), color: TABLER_COLORS.red },
            ]}
          />
        </div>
      )}
      {data && (
        <div className="table-responsive">
          <table className="table table-vcenter card-table">
            <thead>
              <tr>
                <th>{t("reports.colOrganization")}</th>
                <th>{t("reports.colFacilities")}</th>
                <th>{t("reports.colWorkOrdersOpenOverdue")}</th>
                <th>{t("reports.colPmOnTimeRate")}</th>
                <th>{t("reports.colTotalCost")}</th>
                <th>{t("reports.colChargeback")}</th>
              </tr>
            </thead>
            <tbody>
              {data.data?.map((row) => (
                <tr key={row.org_id}>
                  <td style={{ paddingLeft: `${(row.depth ?? 0) * 16 + 12}px` }}>{row.org_name}</td>
                  <td className="text-secondary">{row.facility_count}</td>
                  <td>
                    {row.work_orders_open} / <span className={row.work_orders_overdue ? "text-danger" : ""}>{row.work_orders_overdue}</span>
                  </td>
                  <td>
                    <PercentBar value={row.pm_on_time_rate != null ? row.pm_on_time_rate * 100 : null} />
                  </td>
                  <td>{row.total_cost?.toLocaleString()}</td>
                  <td className="text-secondary">{row.chargeback_cost?.toLocaleString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {data.data?.length === 0 && <EmptyState title={t("reports.noOrganizationsInScope")} />}
        </div>
      )}
    </div>
  );
}
