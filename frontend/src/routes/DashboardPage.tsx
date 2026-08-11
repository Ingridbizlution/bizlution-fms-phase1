import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  IconAlertOctagon,
  IconBellRinging,
  IconBuildingWarehouse,
  IconClipboardList,
  IconClockHour4,
  IconGauge,
  IconTool,
  IconWifiOff,
} from "@tabler/icons-react";
import { getFacilityDashboard } from "../api/reports";
import { useAuth } from "../auth/AuthContext";
import { humanizeEnum } from "../lib/format";
import { DonutChart } from "../shell/charts/DonutChart";
import { CHART_PALETTE } from "../shell/charts/chartTheme";
import { EmptyState } from "../shell/EmptyState";
import { PageBody } from "../shell/PageBody";
import { PageHeader } from "../shell/PageHeader";
import { StatCard } from "../shell/StatCard";

/** Turns a `{[code]: count}` map into chart data, sorted largest-first, colored from the shared palette. */
function toChartData(map: Record<string, number> | undefined, labelFor: (code: string) => string) {
  return Object.entries(map ?? {})
    .filter(([, count]) => (count ?? 0) > 0)
    .sort(([, a], [, b]) => (b ?? 0) - (a ?? 0))
    .map(([code, count], i) => ({ label: labelFor(code), value: count ?? 0, color: CHART_PALETTE[i % CHART_PALETTE.length] }));
}

function pct(value: number | null | undefined): string {
  return value == null ? "—" : `${value.toFixed(1)}%`;
}

/** Joins non-empty sub-line fragments with a middle dot — lets a StatCard's sub text grow with
 *  additional real fields without a fixed-arity template for every combination. */
function joinSub(...parts: (string | null | undefined | false)[]): string {
  return parts.filter(Boolean).join(" · ");
}

export function DashboardPage() {
  const { t } = useTranslation();
  const { currentUser, facilityId } = useAuth();
  const facilityName = currentUser?.accessible_facilities?.find((f) => f.id === facilityId)?.name;

  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["facility-dashboard", facilityId],
    queryFn: () => getFacilityDashboard(facilityId!, "7d"),
    enabled: !!facilityId,
  });

  return (
    <>
      <PageHeader
        pretitle={currentUser?.tenant?.name}
        title={facilityName ? t("dashboard.overviewOf", { facility: facilityName }) : t("dashboard.overview")}
      />
      <PageBody>
        {!facilityId && <EmptyState title={t("dashboard.noFacilitySelected")} subtitle={t("dashboard.pickFacility")} />}

        {isLoading && facilityId && (
          <div className="d-flex justify-content-center py-5">
            <div className="spinner-border text-primary" role="status" aria-label={t("dashboard.loadingDashboard")} />
          </div>
        )}

        {isError && (
          <div className="alert alert-danger">{t("dashboard.loadError", { message: (error as Error).message })}</div>
        )}

        {data && (
          <div className="row row-deck row-cards g-3">
            <div className="col-6 col-md-3">
              <StatCard
                icon={IconClipboardList}
                label={t("dashboard.openWorkOrders")}
                value={data.work_orders?.open ?? 0}
                sub={joinSub(
                  t("dashboard.overdueCount", { count: data.work_orders?.overdue ?? 0 }),
                  (data.work_orders?.completed_in_period ?? 0) > 0 && t("dashboard.completedInPeriod", { count: data.work_orders?.completed_in_period }),
                )}
                tone={data.work_orders?.overdue ? "warn" : "default"}
              />
            </div>
            <div className="col-6 col-md-3">
              <StatCard icon={IconGauge} label={t("dashboard.slaCompliance")} value={pct(data.sla?.compliance_pct)} sub={t("dashboard.slaSub", { breached: data.sla?.breached ?? 0, atRisk: data.sla?.at_risk ?? 0 })} tone={(data.sla?.breached ?? 0) > 0 ? "critical" : "good"} />
            </div>
            <div className="col-6 col-md-3">
              <StatCard
                icon={IconAlertOctagon}
                label={t("dashboard.assetsDown")}
                value={data.assets?.down ?? 0}
                sub={joinSub(
                  t("dashboard.assetsDownSub", { degraded: data.assets?.degraded ?? 0, total: data.assets?.total ?? 0 }),
                  (data.assets?.warranty_expiring_90d ?? 0) > 0 && t("dashboard.warrantyExpiring", { count: data.assets?.warranty_expiring_90d }),
                  data.assets?.avg_health_score != null && t("dashboard.avgHealthScore", { score: Math.round(data.assets.avg_health_score) }),
                )}
                tone={(data.assets?.down ?? 0) > 0 ? "critical" : "default"}
              />
            </div>
            <div className="col-6 col-md-3">
              <StatCard
                icon={IconBellRinging}
                label={t("dashboard.activeAlarms")}
                value={data.alarms?.active ?? 0}
                sub={joinSub(
                  t("dashboard.criticalCount", { count: data.alarms?.critical ?? 0 }),
                  (data.alarms?.unlinked_to_work_order ?? 0) > 0 && t("dashboard.unlinkedAlarms", { count: data.alarms?.unlinked_to_work_order }),
                )}
                tone={(data.alarms?.critical ?? 0) > 0 ? "critical" : "default"}
              />
            </div>
            <div className="col-6 col-md-3">
              <StatCard
                icon={IconTool}
                label={t("dashboard.pmCompliance")}
                value={pct(data.maintenance?.pm_compliance_pct)}
                sub={joinSub(
                  t("dashboard.pmDueSub", { count: data.maintenance?.pm_due_30d ?? 0 }),
                  (data.maintenance?.overdue_occurrences ?? 0) > 0 && t("dashboard.pmOverdueOccurrences", { count: data.maintenance?.overdue_occurrences }),
                )}
              />
            </div>
            <div className="col-6 col-md-3">
              <StatCard
                icon={IconBuildingWarehouse}
                label={t("dashboard.spaceUtilization")}
                value={pct(data.space?.utilization_pct)}
                sub={joinSub(
                  t("dashboard.bookableResourcesSub", { count: data.space?.bookable_resources ?? 0 }),
                  data.space?.no_show_pct != null && t("dashboard.noShowRate", { pct: data.space.no_show_pct.toFixed(1) }),
                )}
              />
            </div>
            <div className="col-6 col-md-3">
              <StatCard
                icon={IconWifiOff}
                label={t("dashboard.devicesOffline")}
                value={data.devices?.offline ?? 0}
                sub={joinSub(
                  t("dashboard.ofTotalSub", { count: data.devices?.total ?? 0 }),
                  (data.devices?.stale_over_24h ?? 0) > 0 && t("dashboard.staleDevices", { count: data.devices?.stale_over_24h }),
                )}
                tone={(data.devices?.offline ?? 0) > 0 ? "warn" : "default"}
              />
            </div>
            <div className="col-6 col-md-3">
              <StatCard icon={IconClockHour4} label={t("dashboard.avgResolution")} value={data.work_orders?.avg_resolution_minutes != null ? `${Math.round(data.work_orders.avg_resolution_minutes / 60)}h` : "—"} sub={t("dashboard.last7Days")} />
            </div>

            <div className="col-md-4">
              <div className="card">
                <div className="card-header">
                  <h3 className="card-title">{t("dashboard.jumpIn")}</h3>
                </div>
                <div className="card-body d-flex align-items-start gap-2 flex-wrap">
                  <Link to="/work-orders" className="btn btn-outline-primary">
                    {t("dashboard.viewWorkOrders")}
                  </Link>
                  <Link to="/reservations" className="btn btn-outline-primary">
                    {t("dashboard.bookASpace")}
                  </Link>
                  <Link to="/assets" className="btn btn-outline-primary">
                    {t("dashboard.browseAssets")}
                  </Link>
                </div>
              </div>
            </div>

            <div className="col-md-4">
              <div className="card">
                <div className="card-header">
                  <h3 className="card-title">{t("dashboard.workOrdersByStatus")}</h3>
                </div>
                <div className="card-body">
                  {(() => {
                    const byStatus = toChartData(data.work_orders?.by_status, humanizeEnum);
                    if (!byStatus.length) return <p className="text-secondary mb-0">{t("dashboard.noWorkOrdersYet")}</p>;
                    return <DonutChart data={byStatus} height={200} />;
                  })()}
                </div>
              </div>
            </div>

            <div className="col-md-4">
              <div className="card">
                <div className="card-header">
                  <h3 className="card-title">{t("dashboard.workOrdersBySource")}</h3>
                </div>
                <div className="card-body">
                  {(() => {
                    const bySource = toChartData(data.work_orders?.by_source, humanizeEnum);
                    if (!bySource.length) return <p className="text-secondary mb-0">{t("dashboard.noWorkOrdersYet")}</p>;
                    return <DonutChart data={bySource} height={200} />;
                  })()}
                </div>
              </div>
            </div>
          </div>
        )}
      </PageBody>
    </>
  );
}
