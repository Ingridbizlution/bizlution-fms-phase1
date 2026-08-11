import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { getLatestTelemetry, getTelemetrySeries, type TelemetryLatest } from "../../api/iot";
import { AreaChart } from "../../shell/charts/AreaChart";
import { EmptyState } from "../../shell/EmptyState";

function TrendPanel({ point }: { point: TelemetryLatest }) {
  const { t } = useTranslation();
  const to = new Date().toISOString();
  const from = new Date(Date.now() - 24 * 3600_000).toISOString();
  const { data, isLoading } = useQuery({
    queryKey: ["telemetry-series", point.device_id, point.point_code],
    queryFn: () => getTelemetrySeries(point.device_id!, point.point_code!, from, to, "1h"),
  });
  const chartData = data?.items?.map((b) => ({ x: new Date(b.bucket_start!).getTime(), y: b.avg_value ?? null })) ?? [];

  return (
    <div className="mt-2">
      <div className="text-secondary" style={{ fontSize: "0.7rem" }}>{t("iot.last24h")}</div>
      {isLoading ? <span className="text-secondary small">{t("iot.loadingTrend")}</span> : <AreaChart data={chartData} height={70} valueSuffix={point.unit ? ` ${point.unit}` : ""} />}
    </div>
  );
}

export function TelemetryTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const { data, isLoading, isError } = useQuery({
    queryKey: ["telemetry-latest", facilityId],
    queryFn: () => getLatestTelemetry(facilityId),
    refetchInterval: 20_000,
  });

  if (isLoading) {
    return (
      <div className="d-flex justify-content-center py-5">
        <div className="spinner-border text-primary" role="status" aria-label={t("iot.loadingTelemetry")} />
      </div>
    );
  }
  if (isError) return <div className="alert alert-danger">{t("iot.loadTelemetryError")}</div>;
  if (!data?.items?.length) return <EmptyState title={t("iot.noTelemetryPoints")} />;

  return (
    <div className="row row-deck row-cards g-3">
      {data.meta?.stale_count ? <div className="col-12"><div className="alert alert-warning mb-0">{t("iot.staleReadings", { count: data.meta.stale_count })}</div></div> : null}
      {data.items.map((point) => (
        <div className="col-md-4" key={point.telemetry_point_id}>
          <div className="card card-sm">
            <div className="card-body">
              <div className="d-flex justify-content-between">
                <div className="text-secondary">{point.point_name}</div>
                {point.is_stale && <span className="badge bg-yellow-lt">{t("iot.stale")}</span>}
              </div>
              <div className="h2 mb-0">
                {point.value_num ?? (point.value_bool != null ? String(point.value_bool) : point.value_text) ?? "—"} <span className="text-secondary fs-5">{point.unit}</span>
              </div>
              <div className="text-secondary small">{point.device_code} · {new Date(point.observed_at!).toLocaleString()}</div>
              <TrendPanel point={point} />
            </div>
          </div>
        </div>
      ))}
    </div>
  );
}
