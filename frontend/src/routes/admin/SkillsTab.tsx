import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { createSkill, deleteSkill, listSkills, updateSkill, type Skill } from "../../api/admin";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

export function SkillsTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const query = useQuery({ queryKey: ["skills"], queryFn: listSkills });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["skills"] });
  }

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteSkill(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.deleteSkillError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("admin.skillsCatalogueTitle")}</h3>
        <Can permission="team:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("admin.newSkill")}
          </button>
        </Can>
      </div>
      {showForm && (
        <SkillForm
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
              <th>{t("admin.domain")}</th>
              <th>{t("admin.colRequiresCertification")}</th>
              <th>{t("admin.colSource")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {query.data?.items?.map((s) => (
              <Fragment key={s.id}>
                <tr>
                  <td>
                    <code>{s.code}</code>
                  </td>
                  <td>{s.name}</td>
                  <td className="text-secondary">{s.domain}</td>
                  <td>{s.requires_certification ? <span className="badge bg-yellow-lt">{t("common.yes")}</span> : "—"}</td>
                  <td>{s.tenant_id ? <span className="badge bg-blue-lt">{t("admin.tenant")}</span> : <span className="badge bg-secondary-lt">{t("admin.platform")}</span>}</td>
                  <td className="text-end">
                    {s.tenant_id && (
                      <Can permission="team:write">
                        <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === s.id ? null : (s.id ?? null))}>
                          {t("common.edit")}
                        </button>
                        <button
                          type="button"
                          className="btn btn-sm btn-outline-danger"
                          disabled={deleteMutation.isPending}
                          title={t("admin.deleteSkillBlockedHint")}
                          onClick={() => {
                            if (window.confirm(t("admin.confirmDeleteSkill", { name: s.name }))) deleteMutation.mutate(s.id!);
                          }}
                        >
                          {t("common.delete")}
                        </button>
                      </Can>
                    )}
                  </td>
                </tr>
                {editingId === s.id && (
                  <tr>
                    <td colSpan={6} className="bg-body-tertiary">
                      <SkillForm
                        skill={s}
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
      {!query.isLoading && query.data?.items?.length === 0 && <EmptyState title={t("admin.noSkillsCatalogue")} />}
    </div>
  );
}

function SkillForm({ skill, onDone }: { skill?: Skill; onDone: () => void }) {
  const { t } = useTranslation();
  const isEdit = !!skill;
  const [code, setCode] = useState("");
  const [name, setName] = useState(skill?.name ?? "");
  const [domain, setDomain] = useState(skill?.domain ?? "MEP");
  const [requiresCert, setRequiresCert] = useState(skill?.requires_certification ?? false);

  const mutation = useMutation({
    mutationFn: () =>
      isEdit ? updateSkill(skill!.id!, { name, domain, requires_certification: requiresCert }) : createSkill({ code, name, domain, requires_certification: requiresCert }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">
          {mutation.error instanceof ApiError ? (mutation.error.problem.detail ?? mutation.error.message) : t(isEdit ? "admin.saveSkillError" : "admin.createSkillError")}
        </div>
      )}
      <div className="row g-2 align-items-end">
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
        <div className="col-md-3">
          <label className="form-label">{t("admin.domain")}</label>
          <select className="form-select" value={domain} onChange={(e) => setDomain(e.target.value)}>
            <option value="MEP">{t("admin.domainMep")}</option>
            <option value="SAFETY">{t("admin.domainSafety")}</option>
            <option value="SECURITY">{t("admin.domainSecurity")}</option>
            <option value="FABRIC">{t("admin.domainFabric")}</option>
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-check">
            <input type="checkbox" className="form-check-input" checked={requiresCert} onChange={(e) => setRequiresCert(e.target.checked)} />
            <span className="form-check-label">{t("admin.needsCertification")}</span>
          </label>
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending || !name || (!isEdit && !code)} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
