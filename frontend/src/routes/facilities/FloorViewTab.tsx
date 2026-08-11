import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { getFloorView, type FloorViewNode } from "../../api/spatial";
import { EmptyState } from "../../shell/EmptyState";

const RANK_COLOR: Record<number, string> = {
  1: "#7fb3e0", // INFO
  2: "#e8d24c", // WARNING
  3: "#e8a23f", // MINOR
  4: "#e0703f", // MAJOR
  5: "#c94040", // CRITICAL
};

interface BBoxGeometry {
  type?: string;
  min?: [number, number];
  max?: [number, number];
}

function hasBBox(node: FloorViewNode): node is FloorViewNode & { geometry: BBoxGeometry } {
  const g = node.geometry as BBoxGeometry | undefined;
  return !!g && Array.isArray(g.min) && Array.isArray(g.max);
}

export function FloorViewTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const [floor, setFloor] = useState<number | null>(null);
  const [selected, setSelected] = useState<FloorViewNode | null>(null);

  const { data, isLoading, isError } = useQuery({
    queryKey: ["floor-view", facilityId],
    queryFn: () => getFloorView(facilityId),
    refetchInterval: 15_000,
  });

  const floors = data?.meta?.floors ?? [];
  const defaultFloor = useMemo(() => {
    const availableFloors = data?.meta?.floors ?? [];
    if (availableFloors.length === 0) return null;
    const counts = new Map<number, number>();
    for (const n of data?.data ?? []) {
      if (hasBBox(n) && n.floor_level != null) counts.set(n.floor_level, (counts.get(n.floor_level) ?? 0) + 1);
    }
    return [...availableFloors].sort((a, b) => (counts.get(b) ?? 0) - (counts.get(a) ?? 0))[0];
  }, [data]);
  const activeFloor = floor ?? defaultFloor;
  const floorNodes = useMemo(() => (data?.data ?? []).filter((n) => n.floor_level === activeFloor), [data, activeFloor]);
  const withGeometry = floorNodes.filter(hasBBox);
  const withoutGeometryCount = floorNodes.length - withGeometry.length;

  const viewBox = useMemo(() => {
    if (withGeometry.length === 0) return null;
    let minX = Infinity;
    let minY = Infinity;
    let maxX = -Infinity;
    let maxY = -Infinity;
    for (const n of withGeometry) {
      minX = Math.min(minX, n.geometry.min![0]);
      minY = Math.min(minY, n.geometry.min![1]);
      maxX = Math.max(maxX, n.geometry.max![0]);
      maxY = Math.max(maxY, n.geometry.max![1]);
    }
    const pad = Math.max((maxX - minX) * 0.05, 1);
    return { minX: minX - pad, minY: minY - pad, width: maxX - minX + pad * 2, height: maxY - minY + pad * 2, maxY };
  }, [withGeometry]);

  if (isLoading) {
    return (
      <div className="d-flex justify-content-center py-5">
        <div className="spinner-border text-primary" role="status" aria-label={t("facilities.loadingFloorView")} />
      </div>
    );
  }
  if (isError) return <div className="alert alert-danger">{t("facilities.loadFloorViewError")}</div>;

  return (
    <div className="row g-3">
      <div className="col-md-9">
        <div className="card">
          <div className="card-header">
            <div className="btn-group">
              {floors.map((f) => (
                <button key={f} type="button" className={`btn btn-sm ${f === activeFloor ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setFloor(f)}>
                  {f === 0 ? "G" : `F${f}`}
                </button>
              ))}
            </div>
            {withoutGeometryCount > 0 && <span className="text-secondary small ms-auto">{t("facilities.nodesNoGeometry", { count: withoutGeometryCount })}</span>}
          </div>
          <div className="card-body">
            {!viewBox ? (
              <EmptyState title={t("facilities.noGeometryTitle")} subtitle={t("facilities.noGeometrySubtitle")} />
            ) : (
              <>
                <svg viewBox={`${viewBox.minX} ${viewBox.minY} ${viewBox.width} ${viewBox.height}`} style={{ width: "100%", height: 480, background: "var(--tblr-bg-surface-secondary)", border: "1px solid var(--tblr-border-color)" }}>
                  {withGeometry.map((n) => {
                    const g = n.geometry;
                    const x = g.min![0];
                    const y = viewBox.maxY - g.max![1];
                    const w = g.max![0] - g.min![0];
                    const h = g.max![1] - g.min![1];
                    const fill = n.worst_alarm_rank ? RANK_COLOR[n.worst_alarm_rank] : n.occupancy_state === "OCCUPIED" ? "#4bbb95" : "#c8d3dc";
                    const fontSize = Math.max(Math.min(w, h) / 6, viewBox.width / 90);
                    return (
                      <g key={n.id} onClick={() => setSelected(n)} style={{ cursor: "pointer" }}>
                        <rect x={x} y={y} width={w} height={h} fill={fill} opacity={selected?.id === n.id ? 1 : 0.75} stroke="#33506a" strokeWidth={viewBox.width / 300}>
                          <title>{n.name}</title>
                        </rect>
                        <text x={x + w / 2} y={y + h / 2} textAnchor="middle" dominantBaseline="middle" fontSize={fontSize} fill="#1a2b3c" style={{ pointerEvents: "none", userSelect: "none" }}>
                          {n.name}
                        </text>
                        {n.occupancy_state === "OCCUPIED" && (
                          <text x={x + w / 2} y={y + h / 2 + fontSize * 1.2} textAnchor="middle" fontSize={fontSize * 0.75} fill="#1a2b3c" opacity={0.75} style={{ pointerEvents: "none", userSelect: "none" }}>
                            {t("facilities.occupied")}
                          </text>
                        )}
                      </g>
                    );
                  })}
                </svg>
                <div className="d-flex flex-wrap gap-3 mt-2 small text-secondary">
                  {[
                    { labelKey: "facilities.legendVacant", color: "#c8d3dc" },
                    { labelKey: "facilities.legendOccupied", color: "#4bbb95" },
                    { labelKey: "facilities.legendWarning", color: RANK_COLOR[2] },
                    { labelKey: "facilities.legendMajor", color: RANK_COLOR[4] },
                    { labelKey: "facilities.legendCritical", color: RANK_COLOR[5] },
                  ].map((item) => (
                    <span key={item.labelKey} className="d-inline-flex align-items-center gap-1">
                      <span style={{ display: "inline-block", width: 10, height: 10, borderRadius: 2, background: item.color }} />
                      {t(item.labelKey)}
                    </span>
                  ))}
                </div>
              </>
            )}
          </div>
        </div>
      </div>
      <div className="col-md-3">
        <div className="card">
          <div className="card-header">
            <h3 className="card-title">{selected ? selected.name : t("facilities.selectARoom")}</h3>
          </div>
          <div className="card-body">
            {selected ? (
              <dl className="row mb-0">
                <dt className="col-6">{t("facilities.colType")}</dt>
                <dd className="col-6">{selected.node_type_code}</dd>
                <dt className="col-6">{t("facilities.colAssets")}</dt>
                <dd className="col-6">{selected.asset_count ?? 0}</dd>
                <dt className="col-6">{t("facilities.colOpenWOs")}</dt>
                <dd className="col-6">{selected.open_work_orders ?? 0}</dd>
                <dt className="col-6">{t("facilities.colAlarms")}</dt>
                <dd className="col-6">{selected.active_alarms ?? 0}</dd>
                <dt className="col-6">{t("facilities.colOccupancy")}</dt>
                <dd className="col-6">{selected.occupancy_state ?? "—"}</dd>
              </dl>
            ) : (
              <p className="text-secondary mb-0">{t("facilities.clickRoomHint")}</p>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
