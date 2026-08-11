import Chart from "./ReactApexChart";
import { baseChartOptions } from "./chartTheme";

export interface DonutChartDatum {
  label: string;
  value: number;
  color?: string;
}

/** A labeled donut with a centered total — for single-point-in-time breakdowns (e.g. counts by status). */
export function DonutChart({ data, height = 220, totalLabel }: { data: DonutChartDatum[]; height?: number; totalLabel?: string }) {
  const nonZero = data.filter((d) => d.value > 0);
  if (nonZero.length === 0) {
    return <div className="text-secondary small py-4 text-center">—</div>;
  }

  const options = baseChartOptions({
    labels: nonZero.map((d) => d.label),
    colors: nonZero.some((d) => d.color) ? nonZero.map((d) => d.color ?? "#6b7280") : undefined,
    legend: { position: "bottom" },
    plotOptions: {
      pie: {
        donut: {
          size: "70%",
          labels: {
            show: true,
            total: {
              show: true,
              label: totalLabel ?? "",
              fontSize: "0.75rem",
              formatter: (w) => String(w.globals.seriesTotals.reduce((a: number, b: number) => a + b, 0)),
            },
          },
        },
      },
    },
    dataLabels: { enabled: false },
  });

  return <Chart type="donut" options={options} series={nonZero.map((d) => d.value)} height={height} />;
}
