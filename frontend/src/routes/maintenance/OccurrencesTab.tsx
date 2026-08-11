import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { listMaintenanceOccurrences, skipMaintenanceOccurrence } from "../../api/maintenance";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";
import { useCursorList } from "../../lib/useCursorList";

function occurrenceBadge(status: string | undefined): string {
  switch (status) {
    case "COMPLETED":
      return "bg-green-lt";
    case "SKIPPED":
      return "bg-secondary-lt";
    case "GENERATED":
      return "bg-blue-lt";
    default:
      return "bg-yellow-lt";
  }
}

export function OccurrencesTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { items, isLoading, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(["maintenance-occurrences", facilityId], (cursor) =>
    listMaintenanceOccurrences(facilityId, cursor),
  );

  const skipMutation = useMutation({
    mutationFn: (id: string) => skipMaintenanceOccurrence(id, "Skipped from the FMS console."),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["maintenance-occurrences", facilityId] }),
  });

  return (
    <div className="card">
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("maintenance.colPlan")}</th>
              <th>{t("maintenance.colAsset")}</th>
              <th>{t("maintenance.colScheduledFor")}</th>
              <th>{t("common.status")}</th>
              <th>{t("maintenance.colWorkOrder")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((occ) => (
              <tr key={occ.id}>
                <td>{occ.plan_name}</td>
                <td className="text-secondary">{occ.asset_code ?? "—"}</td>
                <td>
                  {occ.scheduled_for ? new Date(occ.scheduled_for).toLocaleDateString() : "—"}
                  {occ.is_missed && <span className="badge bg-red-lt ms-1">{t("maintenance.missed")}</span>}
                  {occ.is_late && <span className="badge bg-orange-lt ms-1">{t("maintenance.daysLate", { count: occ.days_late })}</span>}
                </td>
                <td>
                  <span className={`badge ${occurrenceBadge(occ.status)}`}>{occ.status}</span>
                </td>
                <td>{occ.work_order_no ?? "—"}</td>
                <td className="text-end">
                  {occ.status === "PLANNED" && (
                    <Can permission="maintenance_plan:write">
                      <button type="button" className="btn btn-sm btn-outline-secondary" disabled={skipMutation.isPending} onClick={() => skipMutation.mutate(occ.id!)}>
                        {t("maintenance.skip")}
                      </button>
                    </Can>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && items.length === 0 && <EmptyState title={t("maintenance.noScheduledOccurrences")} />}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}
