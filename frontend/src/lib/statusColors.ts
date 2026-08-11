/** Tabler's `bg-*-lt` badge tints — see docs/FRONTEND-GETTING-STARTED.md for the status_category enum. */
export function workOrderCategoryBadge(category: string | undefined): string {
  switch (category) {
    case "OPEN":
      return "bg-blue-lt";
    case "IN_PROGRESS":
      return "bg-orange-lt";
    case "WAITING":
      return "bg-yellow-lt";
    case "TERMINAL":
      return "bg-green-lt";
    default:
      return "bg-secondary-lt";
  }
}

export function slaStateBadge(slaState: string | undefined): string {
  if (!slaState) return "bg-secondary-lt";
  if (slaState.includes("BREACHED")) return "bg-red-lt";
  if (slaState.includes("AT_RISK") || slaState.includes("RESPONSE_DUE")) return "bg-yellow-lt";
  return "bg-green-lt";
}

export function reservationStatusBadge(status: string | undefined): string {
  switch (status) {
    case "PENDING_APPROVAL":
      return "bg-yellow-lt";
    case "CONFIRMED":
      return "bg-blue-lt";
    case "CHECKED_IN":
      return "bg-azure-lt";
    case "COMPLETED":
      return "bg-green-lt";
    case "CANCELLED":
    case "REJECTED":
    case "NO_SHOW":
      return "bg-red-lt";
    default:
      return "bg-secondary-lt";
  }
}

export function priorityBadge(priority: string | undefined): string {
  switch (priority) {
    case "CRITICAL":
    case "URGENT":
      return "bg-red-lt";
    case "HIGH":
      return "bg-orange-lt";
    case "MEDIUM":
      return "bg-yellow-lt";
    default:
      return "bg-secondary-lt";
  }
}

/** Same severity scale as priority (CRITICAL/HIGH/MEDIUM/LOW) — kept as a separate
 *  name so call sites read as "this is an asset criticality", not a work order priority. */
export const criticalityBadge = priorityBadge;

export function assetStatusBadge(status: string | undefined): string {
  switch (status) {
    case "ACTIVE":
      return "bg-green-lt";
    case "DEGRADED":
      return "bg-yellow-lt";
    case "DOWN":
      return "bg-red-lt";
    case "MAINTENANCE":
      return "bg-azure-lt";
    case "RETIRED":
      return "bg-secondary-lt";
    default:
      return "bg-secondary-lt";
  }
}

/** Busy-block kind on a resource availability timeline (src/routes/reservations/ResourceTimeline.tsx). */
export function busyKindBadge(kind: string | undefined): string {
  switch (kind) {
    case "RESERVATION":
      return "bg-blue-lt";
    case "HOLD":
      return "bg-yellow-lt";
    case "BLACKOUT":
      return "bg-red-lt";
    case "MAINTENANCE":
      return "bg-azure-lt";
    case "BUFFER":
    default:
      return "bg-secondary-lt";
  }
}

/** Text-color class for a 0-100 health score — mirrors the red/yellow/green bands
 *  used elsewhere (SLA compliance bars, alarm severity). */
export function healthScoreColor(score: number | undefined | null): string {
  if (score == null) return "text-secondary";
  if (score < 60) return "text-danger";
  if (score < 80) return "text-warning";
  return "text-success";
}
