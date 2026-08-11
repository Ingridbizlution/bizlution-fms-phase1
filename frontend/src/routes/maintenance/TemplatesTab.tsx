import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  createMaintenanceTemplate,
  deleteMaintenanceTemplate,
  listMaintenanceTemplates,
  updateMaintenanceTemplate,
  type MaintenanceTemplate,
} from "../../api/maintenance";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

const MAINTENANCE_TYPES = ["PREVENTIVE", "INSPECTION", "CALIBRATION", "DEEP_CLEAN", "STATUTORY", "PREDICTIVE"];

/** 檢查清單在畫面上就是一行一項；送出時轉成後端要的 `[{item: "..."}]`。 */
function checklistFromText(text: string): { item: string }[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((item) => ({ item }));
}

function checklistToText(checklist: MaintenanceTemplate["checklist"]): string {
  if (!Array.isArray(checklist)) return "";
  return checklist
    .map((row) => (typeof row === "object" && row && "item" in row ? String((row as unknown as { item: unknown }).item) : JSON.stringify(row)))
    .join("\n");
}

export function TemplatesTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);
  const { data, isLoading } = useQuery({ queryKey: ["maintenance-templates"], queryFn: listMaintenanceTemplates });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["maintenance-templates"] });
  }

  const toggleActiveMutation = useMutation({
    mutationFn: (tpl: MaintenanceTemplate) => updateMaintenanceTemplate(tpl.id!, { is_active: !tpl.is_active }),
    onSuccess: invalidate,
  });

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteMaintenanceTemplate(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("maintenance.deleteTemplateError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("maintenance.tabTemplates")}</h3>
        <Can permission="maintenance_template:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("maintenance.newTemplate")}
          </button>
        </Can>
      </div>
      {showForm && (
        <TemplateForm
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
              <th>{t("maintenance.code")}</th>
              <th>{t("common.name")}</th>
              <th>{t("common.type")}</th>
              <th>{t("maintenance.colEstMinutes")}</th>
              <th>{t("maintenance.colPlansUsingIt")}</th>
              <th>{t("common.status")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {data?.data?.map((tpl) => (
              <Fragment key={tpl.id}>
                <tr>
                  <td>
                    <code>{tpl.code}</code>
                  </td>
                  <td>{tpl.name}</td>
                  <td className="text-secondary">{tpl.maintenance_type}</td>
                  <td>{tpl.estimated_minutes ?? "—"}</td>
                  <td>{tpl.plan_count ?? 0}</td>
                  <td>
                    <span className={`badge ${tpl.is_active ? "bg-green-lt" : "bg-secondary-lt"}`}>
                      {tpl.is_active ? t("maintenance.active") : t("maintenance.inactive")}
                    </span>
                  </td>
                  <td className="text-end">
                    <Can permission="maintenance_template:write">
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === tpl.id ? null : tpl.id!)}>
                        {t("common.edit")}
                      </button>
                      <button
                        type="button"
                        className={`btn btn-sm me-1 ${tpl.is_active ? "btn-outline-danger" : "btn-outline-success"}`}
                        disabled={toggleActiveMutation.isPending}
                        onClick={() => toggleActiveMutation.mutate(tpl)}
                      >
                        {tpl.is_active ? t("maintenance.deactivate") : t("maintenance.activate")}
                      </button>
                      <button
                        type="button"
                        className="btn btn-sm btn-outline-danger"
                        disabled={deleteMutation.isPending}
                        title={(tpl.plan_count ?? 0) > 0 ? t("maintenance.deleteTemplateBlockedHint") : undefined}
                        onClick={() => {
                          if (window.confirm(t("maintenance.confirmDeleteTemplate", { name: tpl.name }))) deleteMutation.mutate(tpl.id!);
                        }}
                      >
                        {t("common.delete")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {editingId === tpl.id && (
                  <tr>
                    <td colSpan={7} className="bg-body-tertiary">
                      <TemplateForm
                        template={tpl}
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
      {!isLoading && data?.data?.length === 0 && <EmptyState title={t("maintenance.noTemplates")} />}
    </div>
  );
}

function TemplateForm({ template, onDone }: { template?: MaintenanceTemplate; onDone: () => void }) {
  const { t } = useTranslation();
  const isEdit = !!template;
  const [code, setCode] = useState(template?.code ?? "");
  const [name, setName] = useState(template?.name ?? "");
  const [maintenanceType, setMaintenanceType] = useState<string>(template?.maintenance_type ?? "PREVENTIVE");
  const [estimatedMinutes, setEstimatedMinutes] = useState(template?.estimated_minutes ?? 60);
  const [checklistText, setChecklistText] = useState(template ? checklistToText(template.checklist) : "");

  const mutation = useMutation({
    mutationFn: () => {
      const checklist = checklistFromText(checklistText);
      return isEdit
        ? updateMaintenanceTemplate(template!.id!, { name, maintenance_type: maintenanceType, estimated_minutes: estimatedMinutes, checklist })
        : createMaintenanceTemplate({ code, name, maintenance_type: maintenanceType, estimated_minutes: estimatedMinutes, checklist });
    },
    onSuccess: onDone,
  });

  const checklistEmpty = checklistFromText(checklistText).length === 0;

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("maintenance.saveTemplateError")}</div>
      )}
      <div className="row g-2">
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("maintenance.code")}</label>
            <input className="form-control" value={code} onChange={(e) => setCode(e.target.value)} />
          </div>
        )}
        <div className="col-md-3">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("common.type")}</label>
          <select className="form-select" value={maintenanceType} onChange={(e) => setMaintenanceType(e.target.value)}>
            {MAINTENANCE_TYPES.map((mt) => (
              <option value={mt} key={mt}>
                {mt}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("maintenance.colEstMinutes")}</label>
          <input type="number" min={1} max={10080} className="form-control" value={estimatedMinutes} onChange={(e) => setEstimatedMinutes(Number(e.target.value))} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("maintenance.checklistOneItemPerLine")}</label>
          <textarea className="form-control" rows={2} value={checklistText} onChange={(e) => setChecklistText(e.target.value)} />
        </div>
      </div>
      <div className="mt-2 d-flex gap-2">
        <button type="button" className="btn btn-primary" disabled={mutation.isPending || !name || (!isEdit && !code) || checklistEmpty} onClick={() => mutation.mutate()}>
          {t("common.save")}
        </button>
        <button type="button" className="btn btn-outline-secondary" onClick={onDone}>
          {t("common.cancel")}
        </button>
      </div>
    </div>
  );
}
