import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import {
  approveReservation,
  cancelReservation,
  cancelReservationSeries,
  checkInReservation,
  checkOutReservation,
  getReservation,
  rejectReservation,
  updateReservation,
  type CancelSeriesResult,
  type CheckOutResult,
} from "../../api/reservations";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { humanizeEnum } from "../../lib/format";
import { reservationStatusBadge } from "../../lib/statusColors";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

/** Turns a `Date`/local datetime input value into an RFC 3339 string with an explicit offset —
 *  the real backend deserializes start/end as `chrono::DateTime<Utc>` and rejects a bare local-looking string. */
function toRfc3339(localDateTime: string): string {
  return new Date(localDateTime).toISOString();
}

function toLocalInputValue(iso: string | undefined | null): string {
  if (!iso) return "";
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function ReservationDetailPage() {
  const { t } = useTranslation();
  const { reservationId } = useParams<{ reservationId: string }>();
  const queryClient = useQueryClient();
  const navigate = useNavigate();

  const [showEdit, setShowEdit] = useState(false);
  const [cancelReasonPrompt, setCancelReasonPrompt] = useState(false);
  const [cancelReason, setCancelReason] = useState("");
  const [rejectReasonPrompt, setRejectReasonPrompt] = useState(false);
  const [rejectReason, setRejectReason] = useState("");
  const [seriesResult, setSeriesResult] = useState<CancelSeriesResult["data"] | null>(null);
  const [checkOutResult, setCheckOutResult] = useState<CheckOutResult["meta"] | null>(null);

  const { data, isLoading, isError } = useQuery({
    queryKey: ["reservation", reservationId],
    queryFn: () => getReservation(reservationId!),
    enabled: !!reservationId,
  });

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["reservation", reservationId] });

  const checkIn = useMutation({ mutationFn: () => checkInReservation(reservationId!), onSuccess: invalidate });
  const checkOut = useMutation({
    mutationFn: () => checkOutReservation(reservationId!),
    onSuccess: (res) => {
      setCheckOutResult(res.meta ?? null);
      invalidate();
    },
  });
  const approve = useMutation({ mutationFn: () => approveReservation(reservationId!), onSuccess: invalidate });
  const reject = useMutation({
    mutationFn: (reason: string) => rejectReservation(reservationId!, reason),
    onSuccess: () => {
      setRejectReasonPrompt(false);
      invalidate();
    },
  });
  const cancel = useMutation({
    mutationFn: (reason: string) => cancelReservation(reservationId!, reason || undefined),
    onSuccess: () => navigate("/reservations"),
  });
  const cancelSeries = useMutation({
    mutationFn: (reason: string) => cancelReservationSeries(data!.recurrence_group_id!, reason || undefined),
    onSuccess: (res) => setSeriesResult(res.data ?? null),
  });
  const edit = useMutation({
    mutationFn: (patch: Parameters<typeof updateReservation>[1]) => updateReservation(reservationId!, patch),
    onSuccess: () => {
      setShowEdit(false);
      invalidate();
    },
  });

  const anyPending = checkIn.isPending || checkOut.isPending || approve.isPending || reject.isPending || cancel.isPending || cancelSeries.isPending || edit.isPending;
  const anyError = checkIn.error ?? checkOut.error ?? approve.error ?? reject.error ?? cancel.error ?? cancelSeries.error ?? edit.error;

  if (isLoading) {
    return (
      <PageBody>
        <div className="d-flex justify-content-center py-5">
          <div className="spinner-border text-primary" role="status" aria-label={t("reservations.detail.loadingReservation")} />
        </div>
      </PageBody>
    );
  }

  if (isError || !data) {
    return (
      <PageBody>
        <div className="alert alert-danger">{t("reservations.detail.loadError")}</div>
      </PageBody>
    );
  }

  const masked = data.is_private && !data.title;
  const isTerminal = ["CANCELLED", "COMPLETED", "REJECTED", "NO_SHOW"].includes(data.status ?? "");

  return (
    <>
      <PageHeader
        pretitle={data.reservation_no}
        title={masked ? t("reservations.booked") : data.title || data.resource_name || t("reservations.detail.defaultTitle")}
        actions={
          <div className="d-flex gap-2">
            {data.status === "PENDING_APPROVAL" && (
              <Can permission="reservation:approve">
                <button className="btn btn-success" disabled={anyPending} onClick={() => approve.mutate()}>
                  {t("reservations.detail.approve")}
                </button>
                <button className="btn btn-outline-danger" disabled={anyPending} onClick={() => setRejectReasonPrompt((s) => !s)}>
                  {t("reservations.detail.reject")}
                </button>
              </Can>
            )}
            {data.status === "CONFIRMED" && !data.checked_in_at && (
              <button className="btn btn-primary" disabled={anyPending} onClick={() => checkIn.mutate()}>
                {t("reservations.detail.checkIn")}
              </button>
            )}
            {data.checked_in_at && !isTerminal && (
              <button className="btn btn-outline-primary" disabled={anyPending} onClick={() => checkOut.mutate()}>
                {t("reservations.detail.checkOut")}
              </button>
            )}
            {!isTerminal && (
              <button className="btn btn-outline-secondary" disabled={anyPending} onClick={() => setShowEdit((s) => !s)}>
                {t("reservations.detail.edit")}
              </button>
            )}
            {!isTerminal && (
              <button className="btn btn-outline-danger" disabled={anyPending} onClick={() => setCancelReasonPrompt((s) => !s)}>
                {t("reservations.detail.cancel")}
              </button>
            )}
            {data.recurrence_group_id && !isTerminal && (
              <button className="btn btn-outline-danger" disabled={anyPending} onClick={() => cancelSeries.mutate("")}>
                {t("reservations.detail.cancelEntireSeries")}
              </button>
            )}
          </div>
        }
      />
      <PageBody>
        {anyError && <div className="alert alert-danger">{anyError instanceof ApiError ? anyError.problem.detail ?? anyError.message : t("reservations.detail.actionFailed")}</div>}

        {seriesResult && (
          <div className="alert alert-success d-flex justify-content-between align-items-center">
            <span>
              {t("reservations.detail.seriesCancelled", { cancelled: seriesResult.cancelled, total: seriesResult.total_in_series })}
              {(seriesResult.skipped_past ?? 0) > 0 && t("reservations.detail.seriesSkippedPast", { count: seriesResult.skipped_past })}
              {(seriesResult.skipped_terminal ?? 0) > 0 && t("reservations.detail.seriesSkippedTerminal", { count: seriesResult.skipped_terminal })}.
            </span>
            <button className="btn btn-sm btn-outline-success" onClick={() => navigate("/reservations")}>
              {t("reservations.detail.backToReservations")}
            </button>
          </div>
        )}

        {checkOutResult && (
          <div className="alert alert-info">
            {t("reservations.detail.checkedOut", { used: checkOutResult.used_minutes, booked: checkOutResult.booked_minutes })}
            {checkOutResult.slot_released && t("reservations.detail.slotReleased")}
          </div>
        )}

        {rejectReasonPrompt && (
          <div className="card mb-3">
            <div className="card-body">
              <label className="form-label">{t("reservations.detail.rejectReasonLabel")}</label>
              <div className="d-flex gap-2">
                <input className="form-control" value={rejectReason} onChange={(e) => setRejectReason(e.target.value)} autoFocus />
                <button className="btn btn-outline-danger" disabled={!rejectReason.trim() || anyPending} onClick={() => reject.mutate(rejectReason)}>
                  {t("reservations.detail.confirmReject")}
                </button>
                <button className="btn btn-link" onClick={() => setRejectReasonPrompt(false)}>
                  {t("reservations.detail.dismiss")}
                </button>
              </div>
            </div>
          </div>
        )}

        {cancelReasonPrompt && (
          <div className="card mb-3">
            <div className="card-body">
              <label className="form-label">{t("reservations.detail.cancelReasonLabel")}</label>
              <div className="d-flex gap-2">
                <input className="form-control" value={cancelReason} onChange={(e) => setCancelReason(e.target.value)} autoFocus />
                <button className="btn btn-outline-danger" disabled={anyPending} onClick={() => cancel.mutate(cancelReason)}>
                  {t("reservations.detail.confirmCancel")}
                </button>
                <button className="btn btn-link" onClick={() => setCancelReasonPrompt(false)}>
                  {t("reservations.detail.dismiss")}
                </button>
              </div>
            </div>
          </div>
        )}

        {showEdit && (
          <EditReservationForm
            title={data.title ?? ""}
            purpose={data.purpose ?? ""}
            partySize={data.party_size ?? 1}
            startAt={data.start_at}
            endAt={data.end_at}
            saving={edit.isPending}
            error={edit.error instanceof ApiError ? edit.error.problem.detail ?? edit.error.message : null}
            onCancel={() => setShowEdit(false)}
            onSave={(patch) => edit.mutate(patch)}
          />
        )}

        <div className="row g-3">
          <div className="col-md-6 d-flex flex-column gap-3">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("reservations.detail.details")}</h3>
                <span className={`badge ${reservationStatusBadge(data.status)} ms-2`}>{humanizeEnum(data.status)}</span>
              </div>
              <div className="card-body">
                <dl className="row mb-0">
                  <dt className="col-5">{t("reservations.detail.resource")}</dt>
                  <dd className="col-7">{data.resource_name}</dd>
                  <dt className="col-5">{t("reservations.detail.when")}</dt>
                  <dd className="col-7">
                    {data.start_at ? new Date(data.start_at).toLocaleString() : "—"} – {data.end_at ? new Date(data.end_at).toLocaleTimeString() : ""}
                  </dd>
                  <dt className="col-5">{t("reservations.detail.partySize")}</dt>
                  <dd className="col-7">{data.party_size}</dd>
                  {!masked && (
                    <>
                      <dt className="col-5">{t("reservations.detail.purpose")}</dt>
                      <dd className="col-7">{data.purpose ?? "—"}</dd>
                      <dt className="col-5">{t("reservations.detail.organizer")}</dt>
                      <dd className="col-7">{data.organizer?.display_name ?? "—"}</dd>
                    </>
                  )}
                </dl>
              </div>
            </div>

            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("reservations.detail.participants")}</h3>
              </div>
              <div className="card-body">
                {data.participants?.length ? (
                  <ul className="list-unstyled mb-0">
                    {data.participants.map((p, i) => (
                      <li key={i} className="mb-1 d-flex justify-content-between">
                        <span>
                          {p.display_name ?? p.external_email ?? t("reservations.detail.unknown")} {p.role === "OPTIONAL" && <span className="text-secondary">{t("reservations.detail.optional")}</span>}
                        </span>
                        {p.response && <span className="badge bg-secondary-lt">{humanizeEnum(p.response)}</span>}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="text-secondary mb-0">{t("reservations.detail.noParticipants")}</p>
                )}
              </div>
            </div>
          </div>

          <div className="col-md-6">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("reservations.detail.addOnServices")}</h3>
              </div>
              <div className="card-body">
                {data.services?.length ? (
                  <ul className="list-unstyled mb-0">
                    {data.services.map((s) => (
                      <li key={s.id} className="mb-1 d-flex justify-content-between">
                        <span>
                          {s.service_name} × {s.quantity}
                        </span>
                        {s.work_order ? <code>{s.work_order.wo_no}</code> : <span className="text-secondary small">{t("reservations.detail.noWorkOrderYet")}</span>}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="text-secondary mb-0">{t("reservations.detail.noAddOnServices")}</p>
                )}
              </div>
            </div>
          </div>
        </div>
      </PageBody>
    </>
  );
}

function EditReservationForm({
  title,
  purpose,
  partySize,
  startAt,
  endAt,
  saving,
  error,
  onCancel,
  onSave,
}: {
  title: string;
  purpose: string;
  partySize: number;
  startAt: string | undefined;
  endAt: string | undefined;
  saving: boolean;
  error: string | null;
  onCancel: () => void;
  onSave: (patch: { title: string; purpose: string; party_size: number; start_at: string; end_at: string }) => void;
}) {
  const { t } = useTranslation();
  const [form, setForm] = useState({
    title,
    purpose,
    party_size: partySize,
    start_at: toLocalInputValue(startAt),
    end_at: toLocalInputValue(endAt),
  });

  return (
    <div className="card mb-3">
      <div className="card-header">
        <h3 className="card-title">{t("reservations.detail.editForm.title")}</h3>
      </div>
      <div className="card-body">
        {error && <div className="alert alert-danger">{error}</div>}
        <div className="row g-2 align-items-end">
          <div className="col-md-3">
            <label className="form-label">{t("reservations.detail.editForm.titleLabel")}</label>
            <input className="form-control" value={form.title} onChange={(e) => setForm((f) => ({ ...f, title: e.target.value }))} />
          </div>
          <div className="col-md-3">
            <label className="form-label">{t("reservations.detail.editForm.purpose")}</label>
            <input className="form-control" value={form.purpose} onChange={(e) => setForm((f) => ({ ...f, purpose: e.target.value }))} />
          </div>
          <div className="col-md-2">
            <label className="form-label">{t("reservations.detail.editForm.partySize")}</label>
            <input type="number" min={1} className="form-control" value={form.party_size} onChange={(e) => setForm((f) => ({ ...f, party_size: Number(e.target.value) }))} />
          </div>
          <div className="col-md-2">
            <label className="form-label">{t("reservations.detail.editForm.starts")}</label>
            <input type="datetime-local" className="form-control" value={form.start_at} onChange={(e) => setForm((f) => ({ ...f, start_at: e.target.value }))} />
          </div>
          <div className="col-md-2">
            <label className="form-label">{t("reservations.detail.editForm.ends")}</label>
            <input type="datetime-local" className="form-control" value={form.end_at} onChange={(e) => setForm((f) => ({ ...f, end_at: e.target.value }))} />
          </div>
        </div>
        <div className="d-flex gap-2 mt-3">
          <button
            className="btn btn-primary"
            disabled={saving || !form.title || !form.start_at || !form.end_at}
            onClick={() =>
              onSave({
                title: form.title,
                purpose: form.purpose,
                party_size: form.party_size,
                start_at: toRfc3339(form.start_at),
                end_at: toRfc3339(form.end_at),
              })
            }
          >
            {t("reservations.detail.editForm.saveChanges")}
          </button>
          <button className="btn btn-link" onClick={onCancel}>
            {t("reservations.detail.editForm.discard")}
          </button>
        </div>
      </div>
    </div>
  );
}
