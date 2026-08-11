import Chart from "./ReactApexChart";
import { baseChartOptions } from "./chartTheme";

export interface BarSeries {
  name: string;
  data: number[];
  color?: string;
}

/**
 * Grouped/single bar chart for report tables — one bar (or bar-group) per
 * category label, one or more named series. Horizontal is used when there
 * are many categories (e.g. one bar per asset) so labels stay legible.
 */
export function BarChart({
  categories,
  series,
  height = 280,
  horizontal = false,
  stacked = false,
  valueSuffix = "",
}: {
  categories: string[];
  series: BarSeries[];
  height?: number;
  horizontal?: boolean;
  stacked?: boolean;
  valueSuffix?: string;
}) {
  if (categories.length === 0) {
    return <div className="text-secondary small py-4 text-center">—</div>;
  }

  const options = baseChartOptions({
    chart: { type: "bar", stacked },
    colors: series.some((s) => s.color) ? series.map((s) => s.color ?? "#6b7280") : undefined,
    plotOptions: {
      bar: horizontal
        ? { horizontal: true, borderRadius: 3, barHeight: "60%" }
        : { horizontal: false, borderRadius: 3, columnWidth: series.length > 1 ? "60%" : "45%" },
    },
    xaxis: { categories },
    yaxis: horizontal ? undefined : { labels: { formatter: (v: number) => `${v}${valueSuffix}` } },
    tooltip: { y: { formatter: (v: number) => `${v}${valueSuffix}` } },
    legend: series.length > 1 ? { position: "top" } : { show: false },
  });

  return (
    <Chart
      type="bar"
      options={options}
      series={series.map((s) => ({ name: s.name, data: s.data }))}
      height={horizontal ? Math.max(height, categories.length * 32) : height}
    />
  );
}
