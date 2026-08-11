import { useTranslation } from "react-i18next";
import type { ResourceAvailability } from "../../api/reservations";
import { busyKindBadge } from "../../lib/statusColors";
import { humanizeEnum } from "../../lib/format";

const WEEKDAY_KEYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const DEFAULT_START_HOUR = 7;
const DEFAULT_END_HOUR = 21;

function parseOpeningHoursForDate(resource: ResourceAvailability, date: string): { startHour: number; endHour: number } | null {
  const weekday = WEEKDAY_KEYS[new Date(`${date}T00:00:00`).getDay()];
  const hours = resource.opening_hours as Record<string, [string, string][]> | undefined;
  const todays = hours?.[weekday];
  if (!todays?.length) return null;
  const [start, end] = todays[0];
  const [startHour, startMin] = start.split(":").map(Number);
  const [endHour, endMin] = end.split(":").map(Number);
  return { startHour: startHour + startMin / 60, endHour: endHour + endMin / 60 };
}

/** Bounds the timeline by the earliest/latest busy or free block on this date — a pure layout
 *  fallback when opening_hours has no entry for today, never a stand-in for real data. */
function boundsFromBlocks(resource: ResourceAvailability, dayStart: Date): { startHour: number; endHour: number } | null {
  const times = [...(resource.busy ?? []), ...(resource.free_slots ?? [])].flatMap((b) => [b.start_at, b.end_at]).filter((t): t is string => !!t);
  if (times.length === 0) return null;
  const hours = times.map((t) => (new Date(t).getTime() - dayStart.getTime()) / 3600_000);
  return { startHour: Math.max(0, Math.floor(Math.min(...hours))), endHour: Math.min(24, Math.ceil(Math.max(...hours))) };
}

/** The API can return `free_slots` as several adjacent entries (e.g. one per granularity
 *  step) instead of one merged run — merge touching entries so a click's bounds reflect the
 *  full contiguous free stretch, not just whichever array entry happened to be clicked. */
function mergeContiguousSlots(slots: { start_at?: string; end_at?: string }[]): { start_at: string; end_at: string }[] {
  const valid = slots.filter((s): s is { start_at: string; end_at: string } => !!s.start_at && !!s.end_at);
  const sorted = [...valid].sort((a, b) => new Date(a.start_at).getTime() - new Date(b.start_at).getTime());
  const merged: { start_at: string; end_at: string }[] = [];
  for (const s of sorted) {
    const last = merged[merged.length - 1];
    if (last && new Date(last.end_at).getTime() === new Date(s.start_at).getTime()) {
      last.end_at = s.end_at;
    } else {
      merged.push({ ...s });
    }
  }
  return merged;
}

export function ResourceTimeline({ resource, date, onSelectSlot }: { resource: ResourceAvailability; date: string; onSelectSlot: (startAt: string, endAt: string) => void }) {
  const { t } = useTranslation();
  const dayStart = new Date(`${date}T00:00:00`);
  const bounds = parseOpeningHoursForDate(resource, date) ?? boundsFromBlocks(resource, dayStart) ?? { startHour: DEFAULT_START_HOUR, endHour: DEFAULT_END_HOUR };
  const { startHour, endHour } = bounds;
  const span = endHour - startHour;

  function pct(iso: string | undefined): number {
    if (!iso) return 0;
    const hour = (new Date(iso).getTime() - dayStart.getTime()) / 3600_000;
    return Math.min(100, Math.max(0, ((hour - startHour) / span) * 100));
  }

  const hourTicks = Array.from({ length: Math.floor(span) + 1 }, (_, i) => startHour + i).filter((h) => h % 2 === 0 || span <= 8);

  const now = new Date();
  const isToday = now.toDateString() === dayStart.toDateString();
  const nowPct = isToday ? pct(now.toISOString()) : null;

  const freeBlocks = mergeContiguousSlots(resource.free_slots ?? []);

  return (
    <div>
      <div className="position-relative" style={{ height: 12, marginBottom: 2 }}>
        {hourTicks.map((h) => (
          <span key={h} className="text-secondary" style={{ position: "absolute", left: `${((h - startHour) / span) * 100}%`, fontSize: "0.65rem", transform: "translateX(-50%)" }}>
            {String(Math.floor(h)).padStart(2, "0")}:00
          </span>
        ))}
      </div>
      <div
        className="position-relative rounded"
        style={{ height: 40, backgroundColor: "var(--tblr-bg-surface-tertiary)", border: "1px solid var(--tblr-border-color)", overflow: "hidden" }}
      >
        {freeBlocks.map((slot, i) => {
          const left = pct(slot.start_at);
          const width = pct(slot.end_at) - left;
          if (width <= 0) return null;
          return (
            <button
              key={`free-${i}`}
              type="button"
              className="bg-green-lt position-absolute top-0 bottom-0 border-0 p-0"
              style={{ left: `${left}%`, width: `${width}%`, cursor: "pointer" }}
              title={`${new Date(slot.start_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} – ${new Date(slot.end_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}`}
              onClick={() => onSelectSlot(slot.start_at, slot.end_at)}
            />
          );
        })}
        {resource.busy?.map((block, i) => {
          const left = pct(block.start_at);
          const width = pct(block.end_at) - left;
          if (width <= 0) return null;
          return (
            <div
              key={`busy-${i}`}
              className={`${busyKindBadge(block.kind)} position-absolute top-0 bottom-0`}
              style={{ left: `${left}%`, width: `${width}%` }}
              title={`${humanizeEnum(block.kind)}${block.reason ? ` — ${block.reason}` : ""}`}
            />
          );
        })}
        {nowPct != null && <div className="position-absolute top-0 bottom-0 bg-danger" style={{ left: `${nowPct}%`, width: 2 }} />}
      </div>
      <div className="d-flex flex-wrap gap-3 mt-1 small text-secondary">
        <span className="d-inline-flex align-items-center gap-1">
          <span className="bg-green-lt d-inline-block rounded-1" style={{ width: 10, height: 10 }} /> {t("reservations.booking.timelineFree")}
        </span>
        <span className="d-inline-flex align-items-center gap-1">
          <span className="bg-blue-lt d-inline-block rounded-1" style={{ width: 10, height: 10 }} /> {t("reservations.booking.timelineReserved")}
        </span>
        <span className="d-inline-flex align-items-center gap-1">
          <span className="bg-red-lt d-inline-block rounded-1" style={{ width: 10, height: 10 }} /> {t("reservations.booking.timelineBlackout")}
        </span>
      </div>
    </div>
  );
}
