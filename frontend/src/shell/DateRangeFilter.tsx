import { useTranslation } from "react-i18next";
import type { DateRange } from "../api/reports";

export function DateRangeFilter({ value, onChange }: { value: DateRange; onChange: (range: DateRange) => void }) {
  const { t } = useTranslation();
  return (
    <div className="d-flex align-items-end gap-2">
      <div>
        <label className="form-label mb-1">{t("common.from")}</label>
        <input type="date" className="form-control form-control-sm" value={value.from} onChange={(e) => onChange({ ...value, from: e.target.value })} />
      </div>
      <div>
        <label className="form-label mb-1">{t("common.to")}</label>
        <input type="date" className="form-control form-control-sm" value={value.to} onChange={(e) => onChange({ ...value, to: e.target.value })} />
      </div>
    </div>
  );
}

export function defaultRange(days = 30): { from: string; to: string } {
  const to = new Date();
  const from = new Date(to.getTime() - days * 86400_000);
  return { from: from.toISOString().slice(0, 10), to: to.toISOString().slice(0, 10) };
}
