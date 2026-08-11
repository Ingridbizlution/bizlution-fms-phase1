# FMS Frontend

All ten nav modules from the frontend architecture plan (Phases 0–4) are now real
screens — nothing left behind a placeholder. Three items originally trimmed for
scope were backfilled afterwards: asset bulk import (CSV paste, dry-run preview
before commit), BIM element manual mapping (resolve a parsed model's unmatched
elements to a spatial node or asset), and reservation recurring series (book a
daily/weekly repeat from `BookingPage`, cancel the whole series from
`ReservationDetailPage`).

- **Phase 0**: API client, auth/session, the Tabler-based app shell, and the
  permission gate.
- **Phase 1**: a real Dashboard, and full list/detail/create screens for Assets,
  Work Orders, and Reservations.
- **Phase 2**: Facilities/Spatial/BIM (spatial node list, BIM upload with async
  parse-status polling, and a Floor View dashboard rendering room bboxes colored
  by alarm severity/occupancy), Maintenance (PM plans/templates/occurrence log),
  and IoT (device registry, live telemetry with trend sparklines, alarm console,
  alarm rules with dry-run).
- **Phase 3**: Service Catalogue (requestable services), and Admin — Users +
  role assignment, Roles & permission matrix (grouped by module), Identity
  Providers (test connection / sync), Audit log, Notification templates
  (tenant overrides shadow platform defaults), Webhooks (signing secret shown
  once on create), and the Skills catalogue.
- **Phase 4**: Reporting & Analytics — SLA compliance, PM compliance, Group
  rollup, Asset reliability (MTBF/MTTR), Space utilization, and Service volume,
  each with a date-range filter and CSV export; an Export Center polls every
  queued job to completion and hands back a download link.

## Stack

React 18 + TypeScript + Vite, `@tabler/core` for styling/interactive widgets, React
Router, TanStack Query, React Hook Form + Zod. API types are generated from
`../api/openapi.yaml` via `openapi-typescript` — re-run `npm run generate:api-types`
whenever the contract changes.

## Running it locally

```bash
npm install
npm run dev          # http://localhost:5173
```

You need something answering `/api/v1/auth/token` and `/api/v1/auth/me` at
`VITE_API_BASE_URL` (see `.env.development`, defaults to `http://localhost:8080/api/v1`).

**Real backend** — follow `docs/FRONTEND-GETTING-STARTED.md` at the repo root
(`docker compose` up, `MIGRATE_MODE=demo` migrate, `cargo run -p fms-server` with
`CORS_ALLOWED_ORIGINS=http://localhost:5173`).

**Mock backend** — if you don't have Postgres/Redis/MinIO handy, or Docker Hub is
unreachable from where you're running this:

```bash
npm run mock-api      # http://localhost:8080/api/v1
```

`mock-server/server.mjs` is a dependency-free Node script implementing auth plus
enough of assets / work-orders / reservations / reports / notifications / spatial
nodes / BIM models / floor-view / maintenance / devices / telemetry / alarms /
service-items / users / roles / permissions / audit-log / identity-providers /
notification-templates / webhooks / skills / the six analytics reports / report
exports to exercise all five phases, seeded with the same demo accounts and
facilities as `docs/FRONTEND-GETTING-STARTED.md` (tenant `DEMO_GROUP`, password
`Demo1234!` for everyone — `admin.chen` for full access, `user.huang` to see the
permission-gated nav collapse). Its in-memory state resets on restart — restart
it between test runs if you want a clean seed again. BIM model upload and report
exports both simulate the real async pattern end-to-end: a presigned PUT / a
queued export job, then status walking PENDING/UPLOADED → RUNNING/PARSING →
COMPLETED/PARSED over a few seconds while the UI polls, exactly like the real
worker pipelines. It exists only so these phases can be exercised end-to-end
without the full Postgres-backed stack; nothing in `src/` knows it exists, and
it should not be treated as a contract reference — `api/openapi.yaml` is.

## Layout

```
src/
  api/       generated types (schema.d.ts), the fetch client (client.ts), and one
             file per resource (auth.ts, assets.ts, workOrders.ts, reservations.ts,
             reports.ts, notifications.ts, spatial.ts, maintenance.ts, iot.ts,
             serviceItems.ts, admin.ts)
  auth/      AuthContext, permission gate (Can / useCan), ProtectedRoute
  shell/     AppShell, Sidebar, Navbar, PageHeader, PageBody, StatCard, EmptyState,
             LoadMore, Sparkline, PercentBar, DateRangeFilter, ExportButton, nav
             manifest
  lib/       useCursorList (cursor-pagination hook), statusColors
  routes/    LoginPage, DashboardPage, and assets/ work-orders/ reservations/
             facilities/ maintenance/ iot/ catalogue/ admin/ reports/ subfolders
             for their screens
```

`facilities/`, `maintenance/`, `iot/`, `admin/`, and `reports/` each use a single
page component with an in-page tab switcher (Floor view/Nodes/BIM models,
Plans/Occurrences/Templates, Alarms/Devices/Telemetry/Rules,
Users/Roles/Identity/Audit/Templates/Webhooks/Skills, the six reports + Export
Center) rather than separate routes per tab — matches how Work Orders'
board/table toggle already works, and keeps the sidebar at ten items instead
of growing with every sub-screen. The six report tabs share `DateRangeFilter`,
`PercentBar`, and `ExportButton`; queuing an export from any tab adds a job to
`ReportingPage`'s shared list, which the Export Center tab polls.

The API client (`src/api/client.ts`) is the one place that knows about
`Authorization`/`X-Tenant-ID`/`X-Facility-ID`/`X-Request-ID` headers, the 401-refresh-
retry-once flow, and RFC 9457 `problem+json` error parsing. Later modules add their
own `src/api/*.ts` files on top of it — they should never touch `fetch` directly.
