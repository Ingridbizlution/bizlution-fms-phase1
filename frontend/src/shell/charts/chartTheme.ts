import type { ApexOptions } from "apexcharts";

/**
 * Shared ApexCharts styling so every chart in the app matches the
 * Japanese-minimalist Tabler theme (src/theme.css) — muted palette, hairline
 * grid lines, no drop shadows, CJK-aware font stack. Colors below are
 * Tabler's own semantic palette (see node_modules/@tabler/core/dist/css/tabler.min.css),
 * the same one src/lib/statusColors.ts maps status/priority/severity onto via
 * bg-*-lt classes — kept in sync here so chart series colors always match
 * the badge colors used elsewhere for the same categories.
 */
export const TABLER_COLORS = {
  primary: "#4b5a73", // matches --tblr-primary override in theme.css
  blue: "#066fd1",
  azure: "#4299e1",
  indigo: "#4263eb",
  purple: "#ae3ec9",
  pink: "#d6336c",
  red: "#d63939",
  orange: "#f76707",
  yellow: "#f59f00",
  lime: "#74b816",
  green: "#2fb344",
  teal: "#0ca678",
  cyan: "#17a2b8",
  secondary: "#6b7280",
} as const;

const GRID_COLOR = "#e4e4e2"; // --tblr-border-color
const AXIS_TEXT_COLOR = "#8a8d94"; // muted, warm-gray-leaning secondary text
const FONT_FAMILY =
  '"Inter Var", Inter, -apple-system, BlinkMacSystemFont, "PingFang TC", "Hiragino Sans", "Noto Sans TC", "Noto Sans JP", "Segoe UI", Roboto, sans-serif';

/** Default series order — matches the color progression used across Tabler's own demo charts. */
export const CHART_PALETTE = [
  TABLER_COLORS.primary,
  TABLER_COLORS.azure,
  TABLER_COLORS.orange,
  TABLER_COLORS.green,
  TABLER_COLORS.yellow,
  TABLER_COLORS.red,
  TABLER_COLORS.purple,
  TABLER_COLORS.teal,
];

/**
 * Base options shared by every chart in the app. Pass `overrides` for
 * per-chart-type specifics (e.g. plotOptions.bar, labels) — this does a
 * shallow merge one level deep on the top-level ApexOptions keys, which is
 * enough since each wrapper component owns its own `chart`/`xaxis`/etc. keys.
 */
export function baseChartOptions(overrides: ApexOptions = {}): ApexOptions {
  return {
    chart: {
      fontFamily: FONT_FAMILY,
      foreColor: AXIS_TEXT_COLOR,
      toolbar: { show: false },
      animations: { enabled: true, speed: 250 },
      ...overrides.chart,
    },
    colors: overrides.colors ?? CHART_PALETTE,
    grid: {
      borderColor: GRID_COLOR,
      strokeDashArray: 0,
      padding: { top: 0, right: 8, bottom: 0, left: 8 },
      ...overrides.grid,
    },
    tooltip: {
      theme: "light",
      style: { fontFamily: FONT_FAMILY },
      ...overrides.tooltip,
    },
    legend: {
      fontFamily: FONT_FAMILY,
      labels: { colors: AXIS_TEXT_COLOR },
      ...overrides.legend,
    },
    dataLabels: { enabled: false, ...overrides.dataLabels },
    stroke: { width: 2, curve: "smooth", ...overrides.stroke },
    xaxis: {
      axisBorder: { color: GRID_COLOR },
      axisTicks: { color: GRID_COLOR },
      labels: { style: { colors: AXIS_TEXT_COLOR, fontFamily: FONT_FAMILY } },
      ...overrides.xaxis,
    },
    yaxis: {
      labels: { style: { colors: AXIS_TEXT_COLOR, fontFamily: FONT_FAMILY } },
      ...overrides.yaxis,
    },
    ...Object.fromEntries(Object.entries(overrides).filter(([k]) => !["chart", "grid", "tooltip", "legend", "dataLabels", "stroke", "xaxis", "yaxis", "colors"].includes(k))),
  };
}
