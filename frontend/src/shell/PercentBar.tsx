function tone(pct: number): string {
  if (pct >= 95) return "bg-success";
  if (pct >= 80) return "bg-warning";
  return "bg-danger";
}

export function PercentBar({ value }: { value: number | null | undefined }) {
  if (value == null) return <span className="text-secondary">—</span>;
  return (
    <div className="d-flex align-items-center gap-2" style={{ minWidth: 140 }}>
      <div className="progress flex-fill" style={{ height: 6 }}>
        <div className={`progress-bar ${tone(value)}`} style={{ width: `${Math.min(100, Math.max(0, value))}%` }} />
      </div>
      <span className="text-nowrap">{value.toFixed(1)}%</span>
    </div>
  );
}
