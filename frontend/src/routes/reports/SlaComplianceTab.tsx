import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { getSlaCompliance } from "../../api/reports";
import { BarChart } from "../../shell/charts/BarChart";
import { TABLER_COLORS } from "../../shell/charts/chartTheme";
import { DateRangeFilter, defaultRange } from "../../shell/DateRangeFilter";
import { EmptyState } from "../../shell/EmptyState";
import { ExportButton } from "../../shell/ExportButton";
import { PercentBar } from "../../shell/PercentBar";

export function SlaComplianceTab({ onExportQueued }: { onExportQueued: (code: string, id: string) => void }) {
  const { t } = useTranslation();
  const [range, setRange] = useState(defaultRange());
  const [groupBy, setGroupBy] = useState("facility");

  const { data, isLoading, isError } = useQuery({
    queryKey: ["report-sla", range, groupBy],
    queryFn: () => getSlaCompliance({ ...range, groupBy }),
  });

  return (
    <div className="card">
      <div className="card-header flex-wrap gap-2">
        <DateRangeFilter value={range} onChange={setRange} />
        <select className="form-select form-select-sm w-auto" value={groupBy} onChange={(e) => setGroupBy(e.target.value)}>
          <option value="facility">{t("reports.byFacility")}</option>
          <option value="team">{t("reports.byTeam")}</option>
          <option value="priority">{t("reports.byPriority")}</option>
          <option value="service_item">{t("reports.byServiceItem")}</option>
        </select>
        <ExportButton code="sla-compliance" range={range} groupBy={groupBy} onQueued={(id) => onExportQueued(t("reports.tabSla"), id)} />
      </div>
      {isLoading && (
        <div className="d-flex justify-content-center py-5">
          <div className="spinner-border text-primary" role="status" aria-label={t("reports.loadingReport")} />
        </div>
      )}
      {isError && <div className="alert alert-danger m-3">{t("reports.loadReportError")}</div>}
      {data && data.data.length > 0 && (
        <div className="card-body border-bottom">
          <BarChart
            categories={data.data.map((row) => row.group_label ?? "—")}
            series={[
              { name: t("reports.colResponseMet"), data: data.data.map((row) => row.response_compliance_pct ?? 0), color: TABLER_COLORS.azure },
              { name: t("reports.colResolutionMet"), data: data.data.map((row) => row.resolution_compliance_pct ?? 0), color: TABLER_COLORS.primary },
            ]}
            valueSuffix="%"
          />
        </div>
      )}
      {data && (
        <div className="table-responsive">
          <table className="table table-vcenter card-table">
            <thead>
              <tr>
                <th>{groupBy.replace("_", " ")}</th>
                <th>{t("reports.colResponseMet")}</th>
                <th>{t("reports.colAvgResponse")}</th>
                <th>{t("reports.colResolutionMet")}</th>
                <th>{t("reports.colAvgResolution")}</th>
              </tr>
            </thead>
            <tbody>
              {data.data.map((row) => (
                <tr key={row.group_key ?? row.group_label}>
                  <td>{row.group_label}</td>
                  <td>
                    <PercentBar value={row.response_compliance_pct} />
                    <div className="text-secondary small">
                      {row.response_met}/{row.response_total}
                    </div>
                  </td>
                  <td>{row.avg_response_minutes != null ? t("reports.minutesShort", { count: Math.round(row.avg_response_minutes) }) : "—"}</td>
                  <td>
                    <PercentBar value={row.resolution_compliance_pct} />
                    <div className="text-secondary small">
                      {row.resolution_met}/{row.resolution_total}
                    </div>
                  </td>
                  <td>{row.avg_resolution_minutes != null ? t("reports.hoursShort", { count: Math.round(row.avg_resolution_minutes / 60) }) : "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {data.data.length === 0 && <EmptyState title={t("reports.noWorkOrdersInRange")} />}
        </div>
      )}
    </div>
  );
}
