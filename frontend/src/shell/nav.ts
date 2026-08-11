import type { Icon } from "@tabler/icons-react";
import {
  IconAlertTriangle,
  IconBriefcase,
  IconBuildingWarehouse,
  IconCalendarEvent,
  IconChartBar,
  IconClipboardList,
  IconHome2,
  IconShieldLock,
  IconTool,
} from "@tabler/icons-react";
import type { PermissionScope } from "../auth/permissions";

export interface NavItem {
  to: string;
  /** i18n key under the `nav` namespace — resolved with `t()` at render time. */
  labelKey: string;
  icon: Icon;
  /** Any-scope permission required to see this module — matches the plan's module sheets. */
  permission?: string;
  scope?: PermissionScope;
  /** Per-module icon tint (Tabler semantic palette, see src/shell/charts/chartTheme.ts) — purely
   *  a visual hierarchy cue on the dark sidebar, not tied to any status/severity meaning. */
  accentColor: string;
}

export const NAV_ITEMS: NavItem[] = [
  { to: "/", labelKey: "nav.dashboard", icon: IconHome2, accentColor: "#8a94a6" },
  { to: "/assets", labelKey: "nav.assets", icon: IconTool, permission: "asset:read", accentColor: "#0ca678" },
  { to: "/facilities", labelKey: "nav.facilities", icon: IconBuildingWarehouse, permission: "facility:read", accentColor: "#4299e1" },
  { to: "/maintenance", labelKey: "nav.maintenance", icon: IconClipboardList, permission: "maintenance_plan:read", accentColor: "#f76707" },
  { to: "/catalogue", labelKey: "nav.catalogue", icon: IconBriefcase, permission: "service_item:read", accentColor: "#ae3ec9" },
  { to: "/work-orders", labelKey: "nav.workOrders", icon: IconClipboardList, permission: "work_order:read", accentColor: "#066fd1" },
  { to: "/reservations", labelKey: "nav.reservations", icon: IconCalendarEvent, permission: "reservation:read", accentColor: "#2fb344" },
  { to: "/iot", labelKey: "nav.iot", icon: IconAlertTriangle, permission: "device:read", accentColor: "#d63939" },
  { to: "/admin", labelKey: "nav.admin", icon: IconShieldLock, permission: "user:read", accentColor: "#6b7280" },
  { to: "/reports", labelKey: "nav.reports", icon: IconChartBar, permission: "report:read", accentColor: "#4263eb" },
];
