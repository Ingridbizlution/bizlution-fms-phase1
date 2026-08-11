import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { getPmComplianceReport } from "../../api/reports";
import { BarChart } from "../../shell/charts/BarChart";
import { TABLER_COLORS } from "../../shell/charts/chartTheme";
import { DateRangeFilter, defaultRange } from "../../shell/DateRangeFilter";
import { EmptyState } from "../../shell/EmptyState";
import { ExportButton } from "../../shell/ExportButton";
import { PercentBar } from "../../shell/PercentBar";

export function PmComplianceTab({ onExportQueued }: { onExportQueued: (code: string, id: string) => void }) {
  const { t } = useTranslation();
  const [range, setRange] = useState(defaultRange(90));
  const [groupBy, setGroupBy] = useState("facility");

  const { data, isLoading, isError } = useQuery({
    queryKey: ["report-pm-compliance", range, groupBy],
    queryFn: () => getPmComplianceReport({ ...range, groupBy }),
  });

  return (
    <div className="card">
      <div className="card-header flex-wrap gap-2">
        <DateRangeFilter value={range} onChange={setRange} />
        <select className="form-select form-select-sm w-auto" value={groupBy} onChange={(e) => setGroupBy(e.target.value)}>
          <option value="facility">{t("reports.byFacility")}</option>
          <option value="plan">{t("reports.byPlan")}</option>
          <option value="none">{t("reports.overall")}</option>
        </select>
        <ExportButton code="pm-compliance" range={range} groupBy={groupBy} onQueued={(id) => onExportQueued(t("reports.tabPm"), id)} />
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
            categories={data.data.map((row) => row.group_label ?? t("reports.allPlans"))}
            series={[{ name: t("reports.colOnTimeRate"), data: data.data.map((row) => (row.on_time_rate != null ? row.on_time_rate * 100 : 0)), color: TABLER_COLORS.primary }]}
            valueSuffix="%"
          />
        </div>
      )}
      {data && (
        <div className="table-responsive">
          <table className="table table-vcenter card-table">
            <thead>
              <tr>
                <th>{groupBy === "none" ? t("reports.overall") : groupBy}</th>
                <th>{t("reports.colScheduled")}</th>
                <th>{t("reports.colOnTime")}</th>
                <th>{t("reports.colLate")}</th>
                <th>{t("reports.colMissed")}</th>
                <th>{t("reports.colOnTimeRate")}</th>
              </tr>
            </thead>
            <tbody>
              {data.data?.map((row) => (
                <tr key={row.group_key ?? row.group_label}>
                  <td>{row.group_label ?? t("reports.allPlans")}</td>
                  <td>{row.scheduled_total}</td>
                  <td>{row.completed_on_time}</td>
                  <td>{row.completed_late}</td>
                  <td>{row.missed ? <span className="badge bg-red-lt">{row.missed}</span> : 0}</td>
                  <td>
                    <PercentBar value={row.on_time_rate != null ? row.on_time_rate * 100 : null} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {data.data?.length === 0 && <EmptyState title={t("reports.noScheduledOccurrencesInRange")} />}
        </div>
      )}
    </div>
  );
}
