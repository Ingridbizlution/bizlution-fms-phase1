import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { listBookableResources } from "../../api/bookableResources";
import { BlackoutConflictError, createResourceBlackout, deleteResourceBlackout, listResourceBlackouts, type BlackoutType, type ConflictingReservation } from "../../api/blackouts";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { humanizeEnum } from "../../lib/format";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";

const BLACKOUT_TYPES: BlackoutType[] = ["MAINTENANCE", "HOLIDAY", "RENOVATION", "PRIVATE_EVENT", "EMERGENCY", "OTHER"];

export function BlackoutsTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [rowError, setRowError] = useState<string | null>(null);

  const { items, isLoading, isError, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(["resource-blackouts", facilityId], (cursor) =>
    listResourceBlackouts({ facilityId, cursor }),
  );

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteResourceBlackout(id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["resource-blackouts", facilityId] }),
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.deleteBlackoutError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("facilities.tabBlackouts")}</h3>
        <span className="text-secondary ms-2">{t("facilities.blackoutsSubtitle")}</span>
        <Can permission="blackout:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("facilities.newBlackout")}
          </button>
        </Can>
      </div>
      {showForm && <NewBlackoutForm facilityId={facilityId} onDone={() => setShowForm(false)} />}
      {rowError && (
        <div className="alert alert-danger m-3 mb-0" onClick={() => setRowError(null)}>
          {rowError}
        </div>
      )}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("facilities.colResource")}</th>
              <th>{t("facilities.colWindow")}</th>
              <th>{t("facilities.colType")}</th>
              <th>{t("facilities.colReason")}</th>
              <th>{t("facilities.colWorkOrder")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((b) => (
              <tr key={b.id}>
                <td>{b.resource_name ?? <span className="badge bg-blue-lt">{t("facilities.wholeFacility")}</span>}</td>
                <td className="text-secondary">
                  {b.start_at ? new Date(b.start_at).toLocaleString() : "—"} – {b.end_at ? new Date(b.end_at).toLocaleString() : ""}
                </td>
                <td>{humanizeEnum(b.blackout_type)}</td>
                <td>{b.reason}</td>
                <td>{b.work_order_no ? <code>{b.work_order_no}</code> : <span className="text-secondary">—</span>}</td>
                <td className="text-end">
                  <Can permission="blackout:write">
                    <button
                      type="button"
                      className="btn btn-sm btn-outline-danger"
                      disabled={deleteMutation.isPending}
                      onClick={() => {
                        if (window.confirm(t("facilities.confirmDeleteBlackout"))) deleteMutation.mutate(b.id!);
                      }}
                    >
                      {t("common.delete")}
                    </button>
                  </Can>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && items.length === 0 && <EmptyState title={t("facilities.noUpcomingBlackouts")} />}
      {isError && <div className="alert alert-danger m-3">{t("facilities.loadBlackoutsError")}</div>}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}

function NewBlackoutForm({ facilityId, onDone }: { facilityId: string; onDone: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [resourceId, setResourceId] = useState<string>("");
  const [startAt, setStartAt] = useState("");
  const [endAt, setEndAt] = useState("");
  const [reason, setReason] = useState("");
  const [blackoutType, setBlackoutType] = useState<BlackoutType>("MAINTENANCE");
  const [conflicts, setConflicts] = useState<ConflictingReservation[] | null>(null);

  const resourcesQuery = useQuery({
    queryKey: ["bookable-resources", facilityId],
    queryFn: () => listBookableResources(facilityId, { includeUnbookable: true }),
  });

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["resource-blackouts", facilityId] });

  const mutation = useMutation({
    mutationFn: (acknowledge: boolean) =>
      createResourceBlackout({
        facility_id: facilityId,
        bookable_resource_id: resourceId || null,
        start_at: new Date(startAt).toISOString(),
        end_at: new Date(endAt).toISOString(),
        reason,
        blackout_type: blackoutType,
        acknowledge_conflicting_reservations: acknowledge,
      }),
    onSuccess: () => {
      invalidate();
      onDone();
    },
    onError: (err) => {
      if (err instanceof BlackoutConflictError) setConflicts(err.conflicts);
    },
  });

  const nonConflictError = mutation.error && !(mutation.error instanceof BlackoutConflictError) ? mutation.error : null;

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {nonConflictError && (
        <div className="alert alert-danger">{nonConflictError instanceof ApiError ? nonConflictError.problem.detail ?? nonConflictError.message : t("facilities.createBlackoutError")}</div>
      )}
      {conflicts && (
        <div className="alert alert-warning">
          <div className="mb-2">{t("facilities.overlapsReservations", { count: conflicts.length })}</div>
          {conflicts.length > 0 && (
            <ul className="mb-2">
              {conflicts.map((c, i) => (
                <li key={c.id ?? i}>
                  <code>{c.reservation_no ?? c.id}</code> {c.requested_by ? t("facilities.requestedBy", { name: c.requested_by }) : ""}
                </li>
              ))}
            </ul>
          )}
          <button type="button" className="btn btn-sm btn-warning" disabled={mutation.isPending} onClick={() => mutation.mutate(true)}>
            {t("facilities.createAnyway")}
          </button>
        </div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-3">
          <label className="form-label">{t("facilities.resource")}</label>
          <select className="form-select" value={resourceId} onChange={(e) => setResourceId(e.target.value)}>
            <option value="">{t("facilities.wholeFacility")}</option>
            {resourcesQuery.data?.data?.map((r) => (
              <option value={r.id} key={r.id}>
                {r.display_name ?? r.id}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.starts")}</label>
          <input type="datetime-local" className="form-control" value={startAt} onChange={(e) => setStartAt(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.ends")}</label>
          <input type="datetime-local" className="form-control" value={endAt} onChange={(e) => setEndAt(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("common.type")}</label>
          <select className="form-select" value={blackoutType} onChange={(e) => setBlackoutType(e.target.value as BlackoutType)}>
            {BLACKOUT_TYPES.map((bt) => (
              <option value={bt} key={bt}>
                {humanizeEnum(bt)}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.reason")}</label>
          <input className="form-control" value={reason} onChange={(e) => setReason(e.target.value)} placeholder={t("facilities.reasonPlaceholder")} />
        </div>
        <div className="col-md-1">
          <button
            type="button"
            className="btn btn-primary w-100"
            disabled={mutation.isPending || !startAt || !endAt || !reason}
            onClick={() => {
              setConflicts(null);
              mutation.mutate(false);
            }}
          >
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
