import * as ReactApexChartModule from "react-apexcharts";

// react-apexcharts' CJS bundle ends up double-wrapped through Vite/esbuild's dependency
// pre-bundling for this package (`mod.default.default` is the actual component class, not
// `mod.default`) — unwrap `.default` until we land on something callable, so this stays correct
// regardless of exactly how many interop layers a given esbuild optimize pass adds.
let Chart: unknown = ReactApexChartModule;
while (Chart && typeof Chart !== "function" && "default" in (Chart as Record<string, unknown>)) {
  Chart = (Chart as { default: unknown }).default;
}

export default Chart as typeof import("react-apexcharts").default;
