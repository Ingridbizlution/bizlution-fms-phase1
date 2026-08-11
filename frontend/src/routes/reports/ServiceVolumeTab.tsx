import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { getServiceVolumeReport } from "../../api/reports";
import { BarChart } from "../../shell/charts/BarChart";
import { TABLER_COLORS } from "../../shell/charts/chartTheme";
import { DateRangeFilter, defaultRange } from "../../shell/DateRangeFilter";
import { EmptyState } from "../../shell/EmptyState";
import { ExportButton } from "../../shell/ExportButton";

export function ServiceVolumeTab({ onExportQueued }: { onExportQueued: (code: string, id: string) => void }) {
  const { t } = useTranslation();
  const [range, setRange] = useState(defaultRange());
  const [groupBy, setGroupBy] = useState("service_item");

  const { data, isLoading, isError } = useQuery({
    queryKey: ["report-service-volume", range, groupBy],
    queryFn: () => getServiceVolumeReport({ ...range, groupBy }),
  });

  return (
    <div className="card">
      <div className="card-header flex-wrap gap-2">
        <DateRangeFilter value={range} onChange={setRange} />
        <select className="form-select form-select-sm w-auto" value={groupBy} onChange={(e) => setGroupBy(e.target.value)}>
          <option value="service_item">{t("reports.byServiceItem")}</option>
          <option value="facility">{t("reports.byFacility")}</option>
          <option value="org">{t("reports.byOrganization")}</option>
        </select>
        <ExportButton code="service-volume" range={range} groupBy={groupBy} onQueued={(id) => onExportQueued(t("reports.tabService"), id)} />
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
            categories={data.data.map((row) => row.group_label ?? "—")}
            series={[
              { name: t("reports.colLaborCost"), data: data.data.map((row) => row.labor_cost ?? 0), color: TABLER_COLORS.azure },
              { name: t("reports.colPartsCost"), data: data.data.map((row) => row.parts_cost ?? 0), color: TABLER_COLORS.purple },
            ]}
            stacked
          />
        </div>
      )}
      {data && (
        <div className="table-responsive">
          <table className="table table-vcenter card-table">
            <thead>
              <tr>
                <th>{groupBy.replace("_", " ")}</th>
                <th>{t("reports.colRequests")}</th>
                <th>{t("reports.colCompleted")}</th>
                <th>{t("reports.colLaborCost")}</th>
                <th>{t("reports.colPartsCost")}</th>
                <th>{t("reports.colTotalCost")}</th>
                <th>{t("reports.colChargeback")}</th>
              </tr>
            </thead>
            <tbody>
              {data.data?.map((row) => (
                <tr key={row.group_key ?? row.group_label}>
                  <td>{row.group_label ?? "—"}</td>
                  <td>{row.requests}</td>
                  <td>{row.completed}</td>
                  <td>{row.labor_cost?.toLocaleString()}</td>
                  <td>{row.parts_cost?.toLocaleString()}</td>
                  <td>
                    {row.total_cost?.toLocaleString()}
                    {(row.work_orders_without_rate ?? 0) > 0 && <span className="badge bg-yellow-lt ms-1">{t("reports.lowerBound")}</span>}
                  </td>
                  <td className="text-secondary">{row.chargeback_cost?.toLocaleString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {data.data?.length === 0 && <EmptyState title={t("reports.noServiceRequestsInRange")} />}
        </div>
      )}
    </div>
  );
}
