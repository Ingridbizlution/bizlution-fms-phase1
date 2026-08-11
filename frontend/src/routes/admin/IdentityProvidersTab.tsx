import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  createDirectoryRoleMapping,
  deleteDirectoryRoleMapping,
  deleteIdentityProvider,
  listDirectoryGroups,
  listDirectoryRoleMappings,
  listIdentityProviders,
  listRoles,
  syncIdentityProvider,
  testIdentityProviderConnection,
} from "../../api/admin";
import { ApiError } from "../../api/client";
import { useAuth } from "../../auth/AuthContext";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

export function IdentityProvidersTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [messages, setMessages] = useState<Record<string, string>>({});
  const [rowError, setRowError] = useState<string | null>(null);
  const [expandedMappingsIdpId, setExpandedMappingsIdpId] = useState<string | null>(null);

  const query = useQuery({ queryKey: ["identity-providers"], queryFn: listIdentityProviders });

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteIdentityProvider(id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["identity-providers"] }),
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.deleteIdentityProviderError")),
  });

  const testMutation = useMutation({
    mutationFn: (id: string) => testIdentityProviderConnection(id),
    onSuccess: (res, id) => setMessages((m) => ({ ...m, [id]: res.detail ?? (res.ok ? t("admin.connectionOk") : t("admin.connectionFailed")) })),
  });
  const syncMutation = useMutation({
    mutationFn: (id: string) => syncIdentityProvider(id),
    onSuccess: (res, id) => {
      setMessages((m) => ({ ...m, [id]: t("admin.syncStatus", { status: res.status ?? "queued" }) }));
      queryClient.invalidateQueries({ queryKey: ["identity-providers"] });
    },
  });

  return (
    <div className="card">
      {rowError && (
        <div className="alert alert-danger m-3 mb-0" onClick={() => setRowError(null)}>
          {rowError}
        </div>
      )}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("admin.colProvider")}</th>
              <th>{t("common.type")}</th>
              <th>{t("common.status")}</th>
              <th>{t("admin.colLastSync")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {query.data?.data?.map((idp) => (
              <Fragment key={idp.id}>
                <tr>
                  <td>
                    <code>{idp.code}</code>
                    <div>{idp.name}</div>
                    {messages[idp.id!] && <div className="text-secondary small">{messages[idp.id!]}</div>}
                  </td>
                  <td className="text-secondary">{idp.provider_type}</td>
                  <td>
                    <span className={`badge ${idp.status === "ACTIVE" ? "bg-green-lt" : "bg-secondary-lt"}`}>{idp.status}</span>
                  </td>
                  <td className="text-secondary">{idp.last_sync_at ? new Date(idp.last_sync_at).toLocaleString() : t("admin.never")}</td>
                  <td className="text-end">
                    <button
                      type="button"
                      className="btn btn-sm btn-outline-secondary me-1"
                      onClick={() => setExpandedMappingsIdpId(expandedMappingsIdpId === idp.id ? null : idp.id!)}
                    >
                      {t("admin.directoryMappingsButton")}
                    </button>
                    <Can permission="identity_provider:write">
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" disabled={testMutation.isPending} onClick={() => testMutation.mutate(idp.id!)}>
                        {t("admin.testConnection")}
                      </button>
                      {idp.sync_enabled && (
                        <button type="button" className="btn btn-sm btn-outline-primary me-1" disabled={syncMutation.isPending} onClick={() => syncMutation.mutate(idp.id!)}>
                          {t("admin.syncNow")}
                        </button>
                      )}
                      <button
                        type="button"
                        className="btn btn-sm btn-outline-danger"
                        disabled={deleteMutation.isPending}
                        title={t("admin.deleteIdentityProviderBlockedHint")}
                        onClick={() => {
                          if (window.confirm(t("admin.confirmDeleteIdentityProvider", { name: idp.name }))) deleteMutation.mutate(idp.id!);
                        }}
                      >
                        {t("common.delete")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {expandedMappingsIdpId === idp.id && (
                  <tr>
                    <td colSpan={5} className="bg-body-tertiary">
                      <DirectoryRoleMappings identityProviderId={idp.id!} />
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      </div>
      {!query.isLoading && query.data?.data?.length === 0 && <EmptyState title={t("admin.noIdentityProviders")} subtitle={t("admin.noIdentityProvidersSubtitle")} />}
    </div>
  );
}

function DirectoryRoleMappings({ identityProviderId }: { identityProviderId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { facilityId, currentUser } = useAuth();
  const [groupId, setGroupId] = useState("");
  const [roleCode, setRoleCode] = useState("");
  const [scopeType, setScopeType] = useState<"TENANT" | "FACILITY">("TENANT");
  const [formError, setFormError] = useState<string | null>(null);

  const groupsQuery = useQuery({ queryKey: ["directory-groups", identityProviderId], queryFn: () => listDirectoryGroups(identityProviderId) });
  const mappingsQuery = useQuery({ queryKey: ["directory-role-mappings"], queryFn: listDirectoryRoleMappings });
  const rolesQuery = useQuery({ queryKey: ["roles"], queryFn: listRoles });

  const groupIds = new Set(groupsQuery.data?.data?.map((g) => g.id));
  const mappingsForProvider = mappingsQuery.data?.items?.filter((m) => m.directory_group_id && groupIds.has(m.directory_group_id)) ?? [];

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["directory-role-mappings"] });
  const createMutation = useMutation({
    mutationFn: () =>
      createDirectoryRoleMapping({
        directory_group_id: groupId,
        role_code: roleCode,
        scope_type: scopeType,
        scope_id: scopeType === "FACILITY" ? facilityId ?? undefined : undefined,
        priority: 100,
        is_active: true,
      }),
    onSuccess: () => {
      setFormError(null);
      setGroupId("");
      setRoleCode("");
      invalidate();
    },
    onError: (err) => setFormError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.createDirectoryMappingError")),
  });
  const deleteMutation = useMutation({ mutationFn: (id: string) => deleteDirectoryRoleMapping(id), onSuccess: invalidate });

  return (
    <div className="py-2">
      {formError && (
        <div className="alert alert-danger py-1 px-2 small mb-2" onClick={() => setFormError(null)}>
          {formError}
        </div>
      )}
      <div className="d-flex flex-wrap gap-1 mb-2">
        {mappingsForProvider.map((m) => (
          <span className="badge bg-blue-lt d-inline-flex align-items-center gap-1" key={m.id}>
            {m.directory_group_name} → {m.role_name ?? m.role_code} @ {m.scope_type}
            <Can permission="role:write">
              <button type="button" className="btn-close" style={{ fontSize: 10 }} onClick={() => deleteMutation.mutate(m.id!)} aria-label={t("common.delete")} />
            </Can>
          </span>
        ))}
        {!mappingsForProvider.length && <span className="text-secondary small">{t("admin.noDirectoryMappings")}</span>}
      </div>
      {!groupsQuery.data?.data?.length ? (
        <div className="text-secondary small">{t("admin.noDirectoryGroups")}</div>
      ) : (
        <Can permission="role:write">
          <div className="d-flex flex-wrap gap-1 align-items-center">
            <select className="form-select form-select-sm" style={{ maxWidth: 220 }} value={groupId} onChange={(e) => setGroupId(e.target.value)}>
              <option value="">{t("admin.selectDirectoryGroup")}</option>
              {groupsQuery.data?.data?.map((g) => (
                <option value={g.id} key={g.id}>
                  {g.name}
                </option>
              ))}
            </select>
            <select className="form-select form-select-sm" style={{ maxWidth: 180 }} value={roleCode} onChange={(e) => setRoleCode(e.target.value)}>
              <option value="">{t("admin.selectRole")}</option>
              {rolesQuery.data?.items
                ?.filter((r) => r.is_assignable !== false)
                .map((r) => (
                  <option value={r.code} key={r.code}>
                    {r.name}
                  </option>
                ))}
            </select>
            <select className="form-select form-select-sm" style={{ maxWidth: 160 }} value={scopeType} onChange={(e) => setScopeType(e.target.value as "TENANT" | "FACILITY")}>
              <option value="TENANT">{t("admin.scopeTenant")}</option>
              <option value="FACILITY">{t("admin.scopeFacilityCurrent", { facility: currentUser?.accessible_facilities?.find((f) => f.id === facilityId)?.name })}</option>
            </select>
            <button type="button" className="btn btn-sm btn-primary" disabled={!groupId || !roleCode || createMutation.isPending} onClick={() => createMutation.mutate()}>
              {t("admin.assign")}
            </button>
          </div>
        </Can>
      )}
    </div>
  );
}
