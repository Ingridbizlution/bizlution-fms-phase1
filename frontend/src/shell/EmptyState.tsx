import type { ReactNode } from "react";

export function EmptyState({ title, subtitle, action }: { title: string; subtitle?: string; action?: ReactNode }) {
  return (
    <div className="empty">
      <p className="empty-title">{title}</p>
      {subtitle && <p className="empty-subtitle text-secondary">{subtitle}</p>}
      {action && <div className="empty-action">{action}</div>}
    </div>
  );
}
