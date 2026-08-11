import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { createRole, deleteRole, listPermissions, listRoles, updateRole, type Permission, type Role } from "../../api/admin";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";

export function RolesTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const rolesQuery = useQuery({ queryKey: ["roles"], queryFn: listRoles });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["roles"] });
  }

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteRole(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.deleteRoleError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("admin.rolesTitle")}</h3>
        <Can permission="role:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("admin.newRole")}
          </button>
        </Can>
      </div>
      {showForm && (
        <RoleForm
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
              <th>{t("admin.colRole")}</th>
              <th>{t("admin.colScopeLevel")}</th>
              <th>{t("admin.colPermissions")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {rolesQuery.data?.items?.map((role) => (
              <Fragment key={role.id}>
                <tr>
                  <td>
                    <code>{role.code}</code>
                    <div>{role.name}</div>
                  </td>
                  <td className="text-secondary">{role.scope_level}</td>
                  <td>{role.permissions?.length ?? 0}</td>
                  <td className="text-end">
                    <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setExpanded(expanded === role.code ? null : (role.code ?? null))}>
                      {expanded === role.code ? t("facilities.hide") : t("admin.view")}
                    </button>
                    {!role.is_system && (
                      <Can permission="role:write">
                        <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === role.id ? null : (role.id ?? null))}>
                          {t("common.edit")}
                        </button>
                        <button
                          type="button"
                          className="btn btn-sm btn-outline-danger"
                          disabled={deleteMutation.isPending}
                          title={t("admin.deleteRoleBlockedHint")}
                          onClick={() => {
                            if (window.confirm(t("admin.confirmDeleteRole", { name: role.name }))) deleteMutation.mutate(role.id!);
                          }}
                        >
                          {t("common.delete")}
                        </button>
                      </Can>
                    )}
                  </td>
                </tr>
                {expanded === role.code && (
                  <tr>
                    <td colSpan={4} className="bg-body-tertiary">
                      <div className="d-flex flex-wrap gap-1">
                        {role.permissions?.map((p) => (
                          <code className="badge bg-secondary-lt" key={p}>
                            {p}
                          </code>
                        ))}
                      </div>
                    </td>
                  </tr>
                )}
                {editingId === role.id && (
                  <tr>
                    <td colSpan={4} className="bg-body-tertiary">
                      <RoleForm
                        role={role}
                        onDone={() => {
                          setEditingId(null);
                          void invalidate();
                        }}
                      />
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function RoleForm({ role, onDone }: { role?: Role; onDone: () => void }) {
  const { t } = useTranslation();
  const isEdit = !!role;
  const [code, setCode] = useState("");
  const [name, setName] = useState(role?.name ?? "");
  const [scopeLevel, setScopeLevel] = useState("FACILITY");
  const [isAssignable, setIsAssignable] = useState(role?.is_assignable ?? true);
  const [selected, setSelected] = useState<Set<string>>(new Set(role?.permissions ?? []));

  const permissionsQuery = useQuery({ queryKey: ["permissions"], queryFn: listPermissions });
  const grouped = new Map<string, Permission[]>();
  for (const p of permissionsQuery.data?.items ?? []) {
    const key = p.module ?? "OTHER";
    if (!grouped.has(key)) grouped.set(key, []);
    grouped.get(key)!.push(p);
  }

  const mutation = useMutation({
    mutationFn: () =>
      isEdit
        ? updateRole(role!.id!, { name, is_assignable: isAssignable, permissions: [...selected] })
        : createRole({ code, name, scope_level: scopeLevel, permissions: [...selected] }),
    onSuccess: onDone,
  });

  function toggle(code: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(code)) next.delete(code);
      else next.add(code);
      return next;
    });
  }

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">
          {mutation.error instanceof ApiError ? (mutation.error.problem.detail ?? mutation.error.message) : t(isEdit ? "admin.saveRoleError" : "admin.createRoleError")}
        </div>
      )}
      <div className="row g-2 mb-3">
        {!isEdit && (
          <div className="col-md-3">
            <label className="form-label">{t("admin.code")}</label>
            <input className="form-control" value={code} onChange={(e) => setCode(e.target.value.toUpperCase())} />
          </div>
        )}
        <div className="col-md-3">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        {!isEdit && (
          <div className="col-md-3">
            <label className="form-label">{t("admin.scopeLevel")}</label>
            <select className="form-select" value={scopeLevel} onChange={(e) => setScopeLevel(e.target.value)}>
              <option value="TENANT">{t("admin.scopeTenant")}</option>
              <option value="ORG">{t("admin.scopeOrg")}</option>
              <option value="FACILITY">{t("admin.scopeFacility")}</option>
              <option value="SPATIAL_NODE">{t("admin.scopeSpatialNode")}</option>
            </select>
          </div>
        )}
        {isEdit && (
          <div className="col-md-3 d-flex align-items-end">
            <label className="form-check">
              <input type="checkbox" className="form-check-input" checked={isAssignable} onChange={(e) => setIsAssignable(e.target.checked)} />
              <span className="form-check-label">{t("admin.isAssignable")}</span>
            </label>
          </div>
        )}
      </div>
      <label className="form-label">{t("admin.colPermissions")}</label>
      <div className="row">
        {[...grouped.entries()].map(([module, perms]) => (
          <div className="col-md-4 mb-2" key={module}>
            <div className="text-secondary text-uppercase small mb-1">{module}</div>
            {perms.map((p) => (
              <label className="form-check" key={p.code}>
                <input type="checkbox" className="form-check-input" checked={selected.has(p.code!)} onChange={() => toggle(p.code!)} />
                <span className="form-check-label">
                  <code>{p.code}</code> {p.is_dangerous && <span className="badge bg-red-lt ms-1">{t("admin.dangerous")}</span>}
                </span>
              </label>
            ))}
          </div>
        ))}
      </div>
      <div className="mt-2 d-flex gap-2">
        <button type="button" className="btn btn-primary" disabled={mutation.isPending || !name || (!isEdit && !code)} onClick={() => mutation.mutate()}>
          {isEdit ? t("common.save") : t("admin.saveRole")}
        </button>
        {isEdit && (
          <button type="button" className="btn btn-outline-secondary" onClick={onDone}>
            {t("common.cancel")}
          </button>
        )}
      </div>
    </div>
  );
}
