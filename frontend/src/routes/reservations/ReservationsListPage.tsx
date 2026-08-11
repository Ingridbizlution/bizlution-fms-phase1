import { useState } from "react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { listReservations } from "../../api/reservations";
import { useAuth } from "../../auth/AuthContext";
import { humanizeEnum } from "../../lib/format";
import { reservationStatusBadge } from "../../lib/statusColors";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

const STATUS_OPTIONS = ["PENDING_APPROVAL", "CONFIRMED", "CHECKED_IN", "COMPLETED", "CANCELLED", "REJECTED", "NO_SHOW"];

export function ReservationsListPage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [mine, setMine] = useState(false);
  const [status, setStatus] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");

  const { items, isLoading, isError, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(
    ["reservations", facilityId, mine, status, from, to],
    (cursor) =>
      listReservations({
        facilityId: facilityId ?? undefined,
        mine,
        status: status || undefined,
        from: from ? new Date(`${from}T00:00:00`).toISOString() : undefined,
        to: to ? new Date(`${to}T23:59:59`).toISOString() : undefined,
        cursor,
      }),
    { enabled: !!facilityId },
  );

  return (
    <>
      <PageHeader
        title={t("reservations.title")}
        actions={
          <Link to="/reservations/book" className="btn btn-primary">
            {t("reservations.bookASpace")}
          </Link>
        }
      />
      <PageBody>
        <div className="d-flex flex-wrap gap-3 align-items-end mb-3">
          <label className="form-check mb-0">
            <input type="checkbox" className="form-check-input" checked={mine} onChange={(e) => setMine(e.target.checked)} />
            <span className="form-check-label">{t("reservations.mineOnly")}</span>
          </label>
          <div>
            <label className="form-label mb-1">{t("reservations.status")}</label>
            <select className="form-select form-select-sm" value={status} onChange={(e) => setStatus(e.target.value)}>
              <option value="">{t("reservations.allStatuses")}</option>
              {STATUS_OPTIONS.map((s) => (
                <option value={s} key={s}>
                  {humanizeEnum(s)}
                </option>
              ))}
            </select>
          </div>
          <div>
            <label className="form-label mb-1">{t("reservations.from")}</label>
            <input type="date" className="form-control form-control-sm" value={from} onChange={(e) => setFrom(e.target.value)} />
          </div>
          <div>
            <label className="form-label mb-1">{t("reservations.to")}</label>
            <input type="date" className="form-control form-control-sm" value={to} onChange={(e) => setTo(e.target.value)} />
          </div>
        </div>

        <div className="card">
          <div className="table-responsive">
            <table className="table table-vcenter card-table">
              <thead>
                <tr>
                  <th>{t("reservations.colReservation")}</th>
                  <th>{t("reservations.colWhat")}</th>
                  <th>{t("reservations.colResource")}</th>
                  <th>{t("reservations.colWhen")}</th>
                  <th>{t("reservations.colStatus")}</th>
                  <th>{t("reservations.colOrganizer")}</th>
                </tr>
              </thead>
              <tbody>
                {items.map((r) => (
                  <tr key={r.id}>
                    <td>
                      <Link to={`/reservations/${r.id}`}>
                        <code>{r.reservation_no}</code>
                      </Link>
                    </td>
                    <td>{r.is_private && !r.title ? <span className="text-secondary">{t("reservations.booked")}</span> : r.title || <span className="text-secondary">—</span>}</td>
                    <td>{r.resource_name}</td>
                    <td className="text-secondary">
                      {r.start_at ? new Date(r.start_at).toLocaleString() : "—"} – {r.end_at ? new Date(r.end_at).toLocaleTimeString() : ""}
                    </td>
                    <td>
                      <span className={`badge ${reservationStatusBadge(r.status)}`}>{humanizeEnum(r.status)}</span>
                    </td>
                    <td>
                      {r.is_private && !r.title ? (
                        <span className="text-secondary">{t("reservations.booked")}</span>
                      ) : (
                        r.organizer?.display_name ?? "—"
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {!isLoading && items.length === 0 && <EmptyState title={t("reservations.noReservationsFound")} />}
          {isError && <div className="alert alert-danger m-3">{t("reservations.loadError")}</div>}
          <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
        </div>
      </PageBody>
    </>
  );
}
