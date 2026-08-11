import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { acknowledgeAlarm, createWorkOrderFromAlarm, listAlarms, suppressAlarm } from "../../api/iot";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";

function severityBadge(severity: string | undefined): string {
  switch (severity) {
    case "CRITICAL":
      return "bg-red-lt";
    case "MAJOR":
      return "bg-orange-lt";
    case "MINOR":
      return "bg-yellow-lt";
    case "WARNING":
      return "bg-azure-lt";
    default:
      return "bg-secondary-lt";
  }
}

export function AlarmsTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [unlinkedOnly, setUnlinkedOnly] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const { items, isLoading, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(["alarms", facilityId, unlinkedOnly], (cursor) =>
    listAlarms({ facilityId, unlinkedOnly, cursor }),
  );

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["alarms"] });
  const ackMutation = useMutation({ mutationFn: (id: string) => acknowledgeAlarm(id), onSuccess: invalidate, onError: (e) => setError(describe(e)) });
  const suppressMutation = useMutation({
    mutationFn: (id: string) => suppressAlarm(id, 240, "Maintenance window opened from the FMS console."),
    onSuccess: invalidate,
    onError: (e) => setError(describe(e)),
  });
  const createWoMutation = useMutation({
    mutationFn: (id: string) => createWorkOrderFromAlarm(id),
    onSuccess: (wo: { id?: string }) => {
      invalidate();
      if (wo.id) navigate(`/work-orders/${wo.id}`);
    },
    onError: (e) => setError(describe(e)),
  });

  function describe(err: unknown): string {
    return err instanceof ApiError ? err.problem.detail ?? err.message : t("iot.actionFailed");
  }

  return (
    <div className="card">
      <div className="card-header">
        <label className="form-check">
          <input type="checkbox" className="form-check-input" checked={unlinkedOnly} onChange={(e) => setUnlinkedOnly(e.target.checked)} />
          <span className="form-check-label">{t("iot.unlinkedOnly")}</span>
        </label>
      </div>
      {error && <div className="alert alert-danger m-3 mb-0">{error}</div>}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("iot.colAlarm")}</th>
              <th>{t("iot.colSeverity")}</th>
              <th>{t("common.status")}</th>
              <th>{t("iot.colAssetLocation")}</th>
              <th>{t("iot.colWorkOrder")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((alarm) => (
              <tr key={alarm.id}>
                <td>
                  <code>{alarm.alarm_no}</code>
                  <div className="text-secondary small">{alarm.message}</div>
                </td>
                <td>
                  <span className={`badge ${severityBadge(alarm.severity)}`}>{alarm.severity}</span>
                </td>
                <td>{alarm.status}</td>
                <td className="text-secondary">{alarm.asset?.name ?? alarm.location?.name ?? "—"}</td>
                <td>{alarm.work_order ? <code>{alarm.work_order.wo_no}</code> : <span className="text-secondary">{t("iot.noWorkOrder")}</span>}</td>
                <td className="text-end">
                  <Can permission="alarm:acknowledge">
                    {alarm.status === "ACTIVE" && (
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" disabled={ackMutation.isPending} onClick={() => ackMutation.mutate(alarm.id!)}>
                        {t("iot.acknowledge")}
                      </button>
                    )}
                  </Can>
                  <Can permission="alarm:suppress">
                    {alarm.status !== "SUPPRESSED" && alarm.status !== "CLEARED" && (
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" disabled={suppressMutation.isPending} onClick={() => suppressMutation.mutate(alarm.id!)}>
                        {t("iot.suppress4h")}
                      </button>
                    )}
                  </Can>
                  {!alarm.work_order && (
                    <Can permission="work_order:create">
                      <button type="button" className="btn btn-sm btn-outline-primary" disabled={createWoMutation.isPending} onClick={() => createWoMutation.mutate(alarm.id!)}>
                        {t("iot.createWorkOrder")}
                      </button>
                    </Can>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && items.length === 0 && <EmptyState title={t("iot.noAlarms")} />}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}
