import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { ApiError } from "../../api/client";
import { createHold, createReservation, getAvailability, releaseHold, type ResourceAvailability } from "../../api/reservations";
import { useAuth } from "../../auth/AuthContext";
import { EmptyState } from "../../shell/EmptyState";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";
import { ResourceTimeline } from "./ResourceTimeline";

function tomorrowIso(): string {
  const d = new Date();
  d.setDate(d.getDate() + 1);
  return d.toISOString().slice(0, 10);
}

/** Whole calendar days between today and `dateStr` (local time) — for `rules.advance_booking_days`. */
function daysUntil(dateStr: string): number {
  const target = new Date(`${dateStr}T00:00:00`);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  return Math.round((target.getTime() - today.getTime()) / 86_400_000);
}

function toTimeInputValue(d: Date): string {
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

function withTimeOfDay(base: Date, hhmm: string): Date {
  const [h, m] = hhmm.split(":").map(Number);
  const d = new Date(base);
  d.setHours(h, m, 0, 0);
  return d;
}

interface SelectedSlot {
  resource: ResourceAvailability;
  /** The full contiguous free block the user clicked — not necessarily the final reservation window. */
  blockStartAt: string;
  blockEndAt: string;
}

const REPEAT_OPTIONS = [
  { value: "", labelKey: "reservations.booking.repeatNone" },
  { value: "FREQ=DAILY;COUNT=5", labelKey: "reservations.booking.repeatDaily5" },
  { value: "FREQ=WEEKLY;COUNT=4", labelKey: "reservations.booking.repeatWeekly4" },
  { value: "FREQ=WEEKLY;COUNT=12", labelKey: "reservations.booking.repeatWeekly12" },
];

function BookingForm({ slot, onDone, onCancel }: { slot: SelectedSlot; onDone: () => void; onCancel: () => void }) {
  const { t } = useTranslation();
  const rules = slot.resource.rules;
  const blockStart = new Date(slot.blockStartAt);
  const blockEnd = new Date(slot.blockEndAt);
  const granularityMin = rules?.slot_granularity_minutes ?? 15;
  const defaultDurationMin = Math.max(1, Math.min(rules?.min_duration_minutes ?? 30, (blockEnd.getTime() - blockStart.getTime()) / 60_000));

  const [title, setTitle] = useState("");
  const [partySize, setPartySize] = useState(1);
  const [recurrenceRule, setRecurrenceRule] = useState("");
  const [startAt, setStartAt] = useState(blockStart);
  const [endAt, setEndAt] = useState(new Date(blockStart.getTime() + defaultDurationMin * 60_000));
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const windowError = (() => {
    if (startAt < blockStart || endAt > blockEnd) return t("reservations.booking.outsideAvailableWindow");
    const durationMin = (endAt.getTime() - startAt.getTime()) / 60_000;
    if (durationMin <= 0) return t("reservations.booking.endBeforeStart");
    if (rules?.min_duration_minutes && durationMin < rules.min_duration_minutes) return t("reservations.booking.durationTooShort", { count: rules.min_duration_minutes });
    if (rules?.max_duration_minutes && durationMin > rules.max_duration_minutes) return t("reservations.booking.durationTooLong", { count: rules.max_duration_minutes });
    const offsetMin = (startAt.getTime() - blockStart.getTime()) / 60_000;
    if (offsetMin % granularityMin !== 0) return t("reservations.booking.notAlignedToGranularity", { count: granularityMin });
    return null;
  })();

  async function submit() {
    setSubmitting(true);
    setError(null);
    let holdToken: string | undefined;
    try {
      const startIso = startAt.toISOString();
      const endIso = endAt.toISOString();
      const hold = await createHold({ resource_id: slot.resource.resource_id!, start_at: startIso, end_at: endIso });
      holdToken = hold.hold_token;
      await createReservation({
        resource_id: slot.resource.resource_id!,
        title,
        party_size: partySize,
        start_at: startIso,
        end_at: endIso,
        is_private: false,
        hold_token: holdToken,
        ...(recurrenceRule ? { recurrence_rule: recurrenceRule } : {}),
      });
      onDone();
    } catch (err) {
      if (holdToken) await releaseHold(holdToken).catch(() => undefined);
      setError(err instanceof ApiError ? err.problem.detail ?? err.message : t("reservations.booking.bookingFailed"));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div className="card card-md mt-2">
      <div className="card-body">
        <h4>{slot.resource.display_name}</h4>
        {error && <div className="alert alert-danger">{error}</div>}
        <div className="row g-2 mb-2">
          <div className="col-auto">
            <label className="form-label">{t("reservations.booking.startTime")}</label>
            <input type="time" step={60} className="form-control" value={toTimeInputValue(startAt)} onChange={(e) => setStartAt(withTimeOfDay(blockStart, e.target.value))} />
          </div>
          <div className="col-auto">
            <label className="form-label">{t("reservations.booking.endTime")}</label>
            <input type="time" step={60} className="form-control" value={toTimeInputValue(endAt)} onChange={(e) => setEndAt(withTimeOfDay(blockStart, e.target.value))} />
          </div>
        </div>
        {windowError && <div className="text-danger small mb-2">{windowError}</div>}
        {rules?.requires_approval && <div className="text-secondary small mb-2">{t("reservations.booking.requiresApprovalNotice")}</div>}
        <div className="mb-2">
          <label className="form-label">{t("reservations.booking.whatFor")}</label>
          <input className="form-control" value={title} onChange={(e) => setTitle(e.target.value)} placeholder={t("reservations.booking.whatForPlaceholder")} />
        </div>
        <div className="mb-2" style={{ maxWidth: 160 }}>
          <label className="form-label">{t("reservations.booking.partySize")}</label>
          <input type="number" min={1} className="form-control" value={partySize} onChange={(e) => setPartySize(Number(e.target.value))} />
        </div>
        <div className="mb-2" style={{ maxWidth: 220 }}>
          <label className="form-label">{t("reservations.booking.repeat")}</label>
          <select className="form-select" value={recurrenceRule} onChange={(e) => setRecurrenceRule(e.target.value)}>
            {REPEAT_OPTIONS.map((o) => (
              <option value={o.value} key={o.value}>
                {t(o.labelKey)}
              </option>
            ))}
          </select>
        </div>
        <div className="d-flex gap-2">
          <button type="button" className="btn btn-primary" disabled={submitting || !title.trim() || !!windowError} onClick={submit}>
            {submitting ? t("reservations.booking.booking") : t("reservations.booking.confirmBooking")}
          </button>
          <button type="button" className="btn" onClick={onCancel} disabled={submitting}>
            {t("reservations.booking.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}

export function BookingPage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [date, setDate] = useState(tomorrowIso());
  const [selected, setSelected] = useState<SelectedSlot | null>(null);

  // RFC 3339 with an explicit offset — the real backend deserializes from/to as
  // chrono::DateTime<Utc> and rejects a bare "YYYY-MM-DDTHH:mm:ss" with no zone.
  const from = new Date(`${date}T00:00:00`).toISOString();
  const to = new Date(`${date}T23:59:59`).toISOString();

  const { data, isLoading, isError } = useQuery({
    queryKey: ["availability", facilityId, date],
    queryFn: () => getAvailability(facilityId!, { from, to }),
    enabled: !!facilityId,
  });

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ["availability"] });
    queryClient.invalidateQueries({ queryKey: ["reservations"] });
  };

  const resources = data?.data ?? [];

  return (
    <>
      <PageHeader title={t("reservations.booking.title")} />
      <PageBody>
        <div className="mb-3" style={{ maxWidth: 220 }}>
          <label className="form-label">{t("reservations.booking.date")}</label>
          <input
            type="date"
            className="form-control"
            value={date}
            onChange={(e) => {
              setDate(e.target.value);
              setSelected(null);
            }}
          />
        </div>

        {isLoading && (
          <div className="d-flex justify-content-center py-5">
            <div className="spinner-border text-primary" role="status" aria-label={t("reservations.booking.loadingAvailability")} />
          </div>
        )}
        {isError && <div className="alert alert-danger">{t("reservations.booking.loadError")}</div>}
        {!isLoading && resources.length === 0 && <EmptyState title={t("reservations.booking.noBookableResources")} />}

        <div className="row row-deck row-cards g-3">
          {resources.map((resource) => {
            const advanceDays = resource.rules?.advance_booking_days;
            const beyondAdvanceWindow = advanceDays != null && daysUntil(date) > advanceDays;
            return (
              <div className="col-md-6" key={resource.resource_id}>
                <div className="card">
                  <div className="card-header">
                    <h3 className="card-title">{resource.display_name}</h3>
                    {resource.capacity != null && <span className="text-secondary ms-2">{t("reservations.booking.capacity", { count: resource.capacity })}</span>}
                    {resource.rules?.requires_approval && <span className="badge bg-yellow-lt ms-2">{t("reservations.booking.requiresApproval")}</span>}
                  </div>
                  <div className="card-body">
                    {beyondAdvanceWindow ? (
                      <p className="text-secondary mb-0">{t("reservations.booking.beyondAdvanceWindow", { count: advanceDays })}</p>
                    ) : resource.free_slots?.length || resource.busy?.length ? (
                      <ResourceTimeline resource={resource} date={date} onSelectSlot={(blockStartAt, blockEndAt) => setSelected({ resource, blockStartAt, blockEndAt })} />
                    ) : (
                      <p className="text-secondary mb-0">{t("reservations.booking.noFreeSlots")}</p>
                    )}
                    {selected && selected.resource.resource_id === resource.resource_id && (
                      <BookingForm
                        slot={selected}
                        onCancel={() => setSelected(null)}
                        onDone={() => {
                          invalidate();
                          navigate("/reservations");
                        }}
                      />
                    )}
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      </PageBody>
    </>
  );
}
