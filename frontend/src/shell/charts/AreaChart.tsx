import Chart from "./ReactApexChart";
import { baseChartOptions, TABLER_COLORS } from "./chartTheme";

export interface AreaChartPoint {
  x: number | string;
  y: number | null;
}

/** Time-series trend chart — for real historical data (e.g. telemetry buckets), never for synthesized/fake series. */
export function AreaChart({ data, height = 160, color = TABLER_COLORS.primary, valueSuffix = "" }: { data: AreaChartPoint[]; height?: number; color?: string; valueSuffix?: string }) {
  if (data.length === 0) {
    return <div className="text-secondary small py-4 text-center">—</div>;
  }

  const options = baseChartOptions({
    chart: { type: "area", sparkline: { enabled: height < 120 } },
    colors: [color],
    fill: { type: "gradient", gradient: { shadeIntensity: 1, opacityFrom: 0.35, opacityTo: 0.02, stops: [0, 100] } },
    stroke: { width: 2, curve: "smooth" },
    xaxis: { type: typeof data[0]?.x === "number" ? "datetime" : "category" },
    tooltip: {
      x: { format: "MMM d, HH:mm" },
      y: { formatter: (v: number | null) => (v == null ? "—" : `${v}${valueSuffix}`) },
    },
  });

  return <Chart type="area" options={options} series={[{ name: "", data: data.map((d) => ({ x: d.x, y: d.y })) }]} height={height} />;
}
