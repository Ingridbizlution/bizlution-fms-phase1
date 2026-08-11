import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { createOrganization, deleteOrganization, listOrganizations, updateOrganization, type Organization, type OrganizationCreate } from "../../api/organizations";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

const ORG_TYPES = ["GROUP", "COMPANY", "BUSINESS_UNIT", "REGION", "DEPARTMENT", "TEAM"];

export function OrganizationsTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const query = useQuery({ queryKey: ["organizations"], queryFn: listOrganizations });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["organizations"] });
  }

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteOrganization(id),
    onSuccess: (res) => {
      void invalidate();
      const users = res.meta?.users_still_referencing ?? 0;
      if (users > 0) window.alert(t("admin.orgDeletedStillReferenced", { count: users }));
    },
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.deleteOrgError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("admin.orgsTitle")}</h3>
        <Can permission="organization:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("admin.newOrg")}
          </button>
        </Can>
      </div>
      {showForm && (
        <OrganizationForm
          orgs={query.data?.data ?? []}
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
              <th>{t("admin.code")}</th>
              <th>{t("common.name")}</th>
              <th>{t("admin.orgType")}</th>
              <th>{t("admin.costCenter")}</th>
              <th>{t("admin.colFacilities")}</th>
              <th>{t("common.status")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {query.data?.data?.map((org) => (
              <Fragment key={org.id}>
                <tr>
                  <td>
                    <code>{org.code}</code>
                  </td>
                  <td style={{ paddingLeft: `${1 + (org.depth ?? 0) * 1.5}rem` }}>{org.name}</td>
                  <td className="text-secondary">{org.org_type}</td>
                  <td className="text-secondary">{org.cost_center ?? "—"}</td>
                  <td>{org.facility_count ?? 0}</td>
                  <td>
                    <span className={`badge ${org.status === "ACTIVE" ? "bg-green-lt" : "bg-secondary-lt"}`}>{org.status}</span>
                  </td>
                  <td className="text-end">
                    <Can permission="organization:write">
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === org.id ? null : (org.id ?? null))}>
                        {t("common.edit")}
                      </button>
                      <button
                        type="button"
                        className="btn btn-sm btn-outline-danger"
                        disabled={deleteMutation.isPending}
                        title={t("admin.deleteOrgBlockedHint")}
                        onClick={() => {
                          if (window.confirm(t("admin.confirmDeleteOrg", { name: org.name }))) deleteMutation.mutate(org.id!);
                        }}
                      >
                        {t("common.delete")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {editingId === org.id && (
                  <tr>
                    <td colSpan={7} className="bg-body-tertiary">
                      <OrganizationForm
                        org={org}
                        orgs={query.data?.data ?? []}
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
      {!query.isLoading && query.data?.data?.length === 0 && <EmptyState title={t("admin.noOrganizations")} />}
    </div>
  );
}

function OrganizationForm({ org, orgs, onDone }: { org?: Organization; orgs: Organization[]; onDone: () => void }) {
  const { t } = useTranslation();
  const isEdit = !!org;
  const [parentId, setParentId] = useState(org?.parent_id ?? "");
  const [code, setCode] = useState(org?.code ?? "");
  const [name, setName] = useState(org?.name ?? "");
  const [orgType, setOrgType] = useState(org?.org_type ?? "DEPARTMENT");
  const [costCenter, setCostCenter] = useState(org?.cost_center ?? "");
  const [status, setStatus] = useState(org?.status ?? "ACTIVE");

  const mutation = useMutation({
    mutationFn: () =>
      isEdit
        ? updateOrganization(org!.id!, { name, org_type: orgType, cost_center: costCenter || null, status })
        : createOrganization({ parent_id: parentId || undefined, code, name, org_type: orgType as OrganizationCreate["org_type"], cost_center: costCenter || undefined }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? (mutation.error.problem.detail ?? mutation.error.message) : t("admin.saveOrgError")}</div>
      )}
      <div className="row g-2">
        {!isEdit && (
          <div className="col-md-3">
            <label className="form-label">{t("admin.parentOrg")}</label>
            <select className="form-select" value={parentId} onChange={(e) => setParentId(e.target.value)}>
              <option value="">{t("admin.rootOrg")}</option>
              {orgs.map((o) => (
                <option value={o.id} key={o.id}>
                  {o.name}
                </option>
              ))}
            </select>
          </div>
        )}
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("admin.code")}</label>
            <input className="form-control" value={code} onChange={(e) => setCode(e.target.value.toUpperCase())} />
          </div>
        )}
        <div className="col-md-2">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("admin.orgType")}</label>
          <select className="form-select" value={orgType} onChange={(e) => setOrgType(e.target.value)}>
            {ORG_TYPES.map((ot) => (
              <option value={ot} key={ot}>
                {ot}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("admin.costCenter")}</label>
          <input className="form-control" value={costCenter} onChange={(e) => setCostCenter(e.target.value)} />
        </div>
        {isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("common.status")}</label>
            <select className="form-select" value={status} onChange={(e) => setStatus(e.target.value)}>
              <option value="ACTIVE">ACTIVE</option>
              <option value="INACTIVE">INACTIVE</option>
            </select>
          </div>
        )}
        <div className="col-md-1 d-flex align-items-end">
          <button
            type="button"
            className="btn btn-primary w-100"
            disabled={mutation.isPending || !name || (!isEdit && !code)}
            onClick={() => mutation.mutate()}
          >
            {t("common.save")}
          </button>
        </div>
        {isEdit && (
          <div className="col-md-1 d-flex align-items-end">
            <button type="button" className="btn btn-outline-secondary w-100" onClick={onDone}>
              {t("common.cancel")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
