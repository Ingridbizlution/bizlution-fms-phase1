import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  createCalendarResourceMapping,
  deleteCalendarIntegration,
  deleteCalendarResourceMapping,
  listCalendarIntegrations,
  listCalendarResourceMappings,
  listUnresolvedResources,
  registerCalendarIntegration,
  updateCalendarIntegration,
  updateCalendarResourceMapping,
  type CalendarIntegration,
} from "../../api/calendarIntegrations";
import { listSpatialNodes } from "../../api/spatial";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

function statusBadge(status: string | undefined): string {
  switch (status) {
    case "ACTIVE":
      return "bg-green-lt";
    case "ERROR":
      return "bg-red-lt";
    case "REVOKED":
      return "bg-secondary-lt";
    default:
      return "bg-yellow-lt";
  }
}

export function CalendarIntegrationsTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const { data, isLoading } = useQuery({ queryKey: ["calendar-integrations", facilityId], queryFn: () => listCalendarIntegrations(facilityId) });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["calendar-integrations", facilityId] });
  }

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteCalendarIntegration(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.disconnectCalendarError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("facilities.tabCalendar")}</h3>
        <span className="text-secondary ms-2">{t("facilities.calendarSubtitle")}</span>
        <Can permission="calendar_integration:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("facilities.connectCalendar")}
          </button>
        </Can>
      </div>
      {showForm && (
        <RegisterForm
          facilityId={facilityId}
          onDone={() => {
            setShowForm(false);
            void invalidate();
          }}
        />
      )}
      {rowError && (
        <div className="alert alert-danger m-3 mb-0" onClick={() => setRowError(null)}>
          {rowError}
        </div>
      )}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("facilities.colProvider")}</th>
              <th>{t("common.status")}</th>
              <th>{t("facilities.colSyncCron")}</th>
              <th>{t("facilities.colLastSynced")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {data?.data?.map((it) => (
              <Fragment key={it.id}>
                <tr>
                  <td>
                    <code>{it.provider}</code>
                    {it.last_sync_error && <div className="text-danger small">{it.last_sync_error}</div>}
                  </td>
                  <td>
                    <span className={`badge ${statusBadge(it.status)}`}>{it.status}</span>
                  </td>
                  <td className="text-secondary">{it.sync_cron}</td>
                  <td className="text-secondary">{it.last_synced_at ? new Date(it.last_synced_at).toLocaleString() : t("iot.never")}</td>
                  <td className="text-end">
                    <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setExpandedId(expandedId === it.id ? null : it.id!)}>
                      {t("facilities.manageMappings")}
                    </button>
                    <Can permission="calendar_integration:write">
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === it.id ? null : it.id!)}>
                        {t("common.edit")}
                      </button>
                      <button
                        type="button"
                        className="btn btn-sm btn-outline-danger"
                        disabled={deleteMutation.isPending}
                        title={t("facilities.disconnectBlockedHint")}
                        onClick={() => {
                          if (window.confirm(t("facilities.confirmDisconnect"))) deleteMutation.mutate(it.id!);
                        }}
                      >
                        {t("facilities.disconnect")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {editingId === it.id && (
                  <tr>
                    <td colSpan={5} className="bg-body-tertiary">
                      <EditForm
                        integration={it}
                        onDone={() => {
                          setEditingId(null);
                          void invalidate();
                        }}
                      />
                    </td>
                  </tr>
                )}
                {expandedId === it.id && (
                  <tr>
                    <td colSpan={5} className="bg-body-tertiary">
                      <MappingsPanel integrationId={it.id!} facilityId={facilityId} />
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && data?.data?.length === 0 && <EmptyState title={t("facilities.noCalendarIntegrations")} />}
    </div>
  );
}

function RegisterForm({ facilityId, onDone }: { facilityId: string; onDone: () => void }) {
  const { t } = useTranslation();
  const [provider, setProvider] = useState<"MS365" | "GOOGLE">("MS365");
  const [msTenantId, setMsTenantId] = useState("");

  const mutation = useMutation({
    mutationFn: () => registerCalendarIntegration(facilityId, { provider, ms_tenant_id: msTenantId || undefined }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("facilities.connectCalendarError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-2">
          <label className="form-label">{t("facilities.colProvider")}</label>
          <select className="form-select" value={provider} onChange={(e) => setProvider(e.target.value as "MS365" | "GOOGLE")}>
            <option value="MS365">MS365</option>
            <option value="GOOGLE">GOOGLE</option>
          </select>
        </div>
        {provider === "MS365" && (
          <div className="col-md-4">
            <label className="form-label">{t("facilities.msTenantId")}</label>
            <input className="form-control" value={msTenantId} onChange={(e) => setMsTenantId(e.target.value)} placeholder={t("facilities.msTenantIdPlaceholder")} />
          </div>
        )}
        <div className="col-md-2">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}

function EditForm({ integration, onDone }: { integration: CalendarIntegration; onDone: () => void }) {
  const { t } = useTranslation();
  const [syncCron, setSyncCron] = useState(integration.sync_cron ?? "*/5 * * * *");

  const mutation = useMutation({
    mutationFn: () => updateCalendarIntegration(integration.id!, { sync_cron: syncCron }),
    onSuccess: onDone,
  });

  return (
    <div>
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("facilities.saveCalendarError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-3">
          <label className="form-label">{t("facilities.colSyncCron")}</label>
          <input className="form-control" value={syncCron} onChange={(e) => setSyncCron(e.target.value)} />
        </div>
        <div className="col-md-2 d-flex gap-1">
          <button type="button" className="btn btn-primary flex-fill" disabled={mutation.isPending || !syncCron} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
          <button type="button" className="btn btn-outline-secondary flex-fill" onClick={onDone}>
            {t("common.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}

function MappingsPanel({ integrationId, facilityId }: { integrationId: string; facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [rowError, setRowError] = useState<string | null>(null);
  const [selectedExternalId, setSelectedExternalId] = useState("");
  const [selectedNodeId, setSelectedNodeId] = useState("");

  const mappingsQuery = useQuery({ queryKey: ["calendar-resource-mappings", integrationId], queryFn: () => listCalendarResourceMappings(integrationId) });
  const unresolvedQuery = useQuery({ queryKey: ["calendar-unresolved-resources", integrationId], queryFn: () => listUnresolvedResources(integrationId) });
  const nodesQuery = useQuery({ queryKey: ["spatial-nodes-picker", facilityId], queryFn: () => listSpatialNodes(facilityId) });

  function invalidateMappings() {
    return Promise.all([
      queryClient.invalidateQueries({ queryKey: ["calendar-resource-mappings", integrationId] }),
      queryClient.invalidateQueries({ queryKey: ["calendar-unresolved-resources", integrationId] }),
    ]);
  }

  const createMutation = useMutation({
    mutationFn: () => {
      const chosen = unresolvedQuery.data?.data?.find((r) => r.external_id === selectedExternalId);
      return createCalendarResourceMapping(integrationId, selectedExternalId, chosen?.display_name, selectedNodeId);
    },
    onSuccess: () => {
      setSelectedExternalId("");
      setSelectedNodeId("");
      void invalidateMappings();
    },
  });

  const patchMutation = useMutation({
    mutationFn: (vars: { id: string; status: string }) => updateCalendarResourceMapping(vars.id, { status: vars.status as "ACTIVE" | "DISABLED" }),
    onSuccess: invalidateMappings,
  });

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteCalendarResourceMapping(id),
    onSuccess: invalidateMappings,
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.deleteMappingError")),
  });

  return (
    <div>
      <h4>{t("facilities.mappings")}</h4>
      {rowError && (
        <div className="alert alert-danger" onClick={() => setRowError(null)}>
          {rowError}
        </div>
      )}
      <table className="table table-sm table-vcenter">
        <thead>
          <tr>
            <th>{t("facilities.colLocation")}</th>
            <th>{t("facilities.colExternalResource")}</th>
            <th>{t("facilities.colSyncDirection")}</th>
            <th>{t("common.status")}</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {mappingsQuery.data?.data?.map((m) => (
            <tr key={m.id}>
              <td>{m.node_name ?? "—"}</td>
              <td className="text-secondary">{m.external_resource_name ?? m.external_resource_id}</td>
              <td className="text-secondary">{m.sync_direction}</td>
              <td>
                <span className={`badge ${m.status === "ACTIVE" ? "bg-green-lt" : "bg-secondary-lt"}`}>{m.status}</span>
              </td>
              <td className="text-end">
                <button
                  type="button"
                  className={`btn btn-sm me-1 ${m.status === "ACTIVE" ? "btn-outline-danger" : "btn-outline-success"}`}
                  disabled={patchMutation.isPending}
                  onClick={() => patchMutation.mutate({ id: m.id, status: m.status === "ACTIVE" ? "DISABLED" : "ACTIVE" })}
                >
                  {m.status === "ACTIVE" ? t("maintenance.deactivate") : t("maintenance.activate")}
                </button>
                <button
                  type="button"
                  className="btn btn-sm btn-outline-danger"
                  disabled={deleteMutation.isPending}
                  title={t("facilities.deleteMappingBlockedHint")}
                  onClick={() => {
                    if (window.confirm(t("facilities.confirmDeleteMapping"))) deleteMutation.mutate(m.id);
                  }}
                >
                  {t("common.delete")}
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {!mappingsQuery.isLoading && mappingsQuery.data?.data?.length === 0 && <div className="text-secondary mb-2">{t("facilities.noMappings")}</div>}

      <div className="row g-2 align-items-end">
        <div className="col-md-4">
          <label className="form-label">{t("facilities.colExternalResource")}</label>
          <select className="form-select" value={selectedExternalId} onChange={(e) => setSelectedExternalId(e.target.value)}>
            <option value="">{t("facilities.selectPlaceholder")}</option>
            {unresolvedQuery.data?.data?.map((r) => (
              <option value={r.external_id} key={r.external_id}>
                {r.display_name ?? r.external_id}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-4">
          <label className="form-label">{t("facilities.colLocation")}</label>
          <select className="form-select" value={selectedNodeId} onChange={(e) => setSelectedNodeId(e.target.value)}>
            <option value="">{t("facilities.selectPlaceholder")}</option>
            {nodesQuery.data?.data?.map((n) => (
              <option value={n.id} key={n.id}>
                {n.name}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <button type="button" className="btn btn-primary w-100" disabled={createMutation.isPending || !selectedExternalId || !selectedNodeId} onClick={() => createMutation.mutate()}>
            {t("facilities.addMapping")}
          </button>
        </div>
      </div>
      {unresolvedQuery.data?.meta?.reason && <div className="text-secondary small mt-2">{unresolvedQuery.data.meta.reason}</div>}
    </div>
  );
}
