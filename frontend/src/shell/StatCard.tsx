import type { Icon } from "@tabler/icons-react";

interface StatCardProps {
  label: string;
  value: string | number;
  sub?: string;
  tone?: "default" | "good" | "warn" | "critical";
  icon?: Icon;
}

const TONE_TEXT: Record<string, string> = {
  good: "text-success",
  warn: "text-warning",
  critical: "text-danger",
  default: "",
};

/** Matches the tint classes statusColors.ts uses for the same tone elsewhere (bg-*-lt badges). */
const TONE_ICON_BG: Record<string, string> = {
  good: "bg-green-lt",
  warn: "bg-yellow-lt",
  critical: "bg-red-lt",
  default: "bg-secondary-lt",
};

export function StatCard({ label, value, sub, tone = "default", icon: IconComponent }: StatCardProps) {
  return (
    <div className="card card-sm">
      <div className="card-body">
        <div className="d-flex align-items-start justify-content-between mb-1">
          <div className="text-secondary">{label}</div>
          {IconComponent && (
            <span className={`avatar avatar-sm rounded ${TONE_ICON_BG[tone]}`}>
              <IconComponent size={18} stroke={1.75} className={TONE_TEXT[tone] || "text-secondary"} />
            </span>
          )}
        </div>
        <div className={`h1 mb-0 ${TONE_TEXT[tone]}`}>{value}</div>
        {sub && <div className="text-secondary small mt-1">{sub}</div>}
      </div>
    </div>
  );
}
