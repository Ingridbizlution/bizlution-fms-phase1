import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { createNotificationTemplate, deleteNotificationTemplate, listNotificationTemplates, updateNotificationTemplate, type NotificationTemplate } from "../../api/admin";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

export function NotificationTemplatesTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const query = useQuery({ queryKey: ["notification-templates"], queryFn: listNotificationTemplates });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["notification-templates"] });
  }

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteNotificationTemplate(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.deleteTemplateError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("admin.tabNotifications")}</h3>
        <Can permission="notification_template:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("admin.newOverride")}
          </button>
        </Can>
      </div>
      {showForm && (
        <NewTemplateForm
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
              <th>{t("admin.colChannel")}</th>
              <th>{t("admin.colLocale")}</th>
              <th>{t("admin.colSource")}</th>
              <th>{t("admin.colActive")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {query.data?.data?.map((tpl) => (
              <Fragment key={tpl.id}>
                <tr>
                  <td>
                    <code>{tpl.code}</code>
                  </td>
                  <td className="text-secondary">{tpl.channel}</td>
                  <td>{tpl.locale}</td>
                  <td>
                    <span className={`badge ${tpl.is_platform ? "bg-secondary-lt" : "bg-blue-lt"}`}>{tpl.is_platform ? t("admin.platform") : t("admin.tenantOverride")}</span>
                    {tpl.is_overridden && <span className="badge bg-yellow-lt ms-1">{t("admin.shadowed")}</span>}
                  </td>
                  <td>{tpl.is_active ? t("common.yes") : t("common.no")}</td>
                  <td className="text-end">
                    {!tpl.is_platform && (
                      <Can permission="notification_template:write">
                        <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === tpl.id ? null : (tpl.id ?? null))}>
                          {t("common.edit")}
                        </button>
                        <button
                          type="button"
                          className="btn btn-sm btn-outline-danger"
                          disabled={deleteMutation.isPending}
                          onClick={() => {
                            if (window.confirm(t("admin.confirmDeleteTemplate", { code: tpl.code }))) deleteMutation.mutate(tpl.id!);
                          }}
                        >
                          {t("common.delete")}
                        </button>
                      </Can>
                    )}
                  </td>
                </tr>
                {editingId === tpl.id && (
                  <tr>
                    <td colSpan={6} className="bg-body-tertiary">
                      <EditTemplateForm
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
      {!query.isLoading && query.data?.data?.length === 0 && <EmptyState title={t("admin.noNotificationTemplates")} />}
    </div>
  );
}

function EditTemplateForm({ template, onDone }: { template: NotificationTemplate; onDone: () => void }) {
  const { t } = useTranslation();
  const [subjectTemplate, setSubjectTemplate] = useState(template.subject_template ?? "");
  const [bodyTemplate, setBodyTemplate] = useState(template.body_template ?? "");
  const [isActive, setIsActive] = useState(template.is_active ?? true);

  const mutation = useMutation({
    mutationFn: () => updateNotificationTemplate(template.id!, { subject_template: subjectTemplate || undefined, body_template: bodyTemplate, is_active: isActive }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("admin.saveTemplateError")}</div>
      )}
      <div className="mb-2">
        <label className="form-label">{t("admin.subjectLabel")}</label>
        <input className="form-control" value={subjectTemplate} onChange={(e) => setSubjectTemplate(e.target.value)} />
      </div>
      <label className="form-label">
        {t("admin.bodyLabel")} ({"{{variable}}"})
      </label>
      <textarea className="form-control mb-2" rows={3} value={bodyTemplate} onChange={(e) => setBodyTemplate(e.target.value)} />
      <label className="form-check mb-2">
        <input type="checkbox" className="form-check-input" checked={isActive} onChange={(e) => setIsActive(e.target.checked)} />
        <span className="form-check-label">{t("admin.colActive")}</span>
      </label>
      <div className="d-flex gap-2">
        <button type="button" className="btn btn-primary" disabled={mutation.isPending || !bodyTemplate} onClick={() => mutation.mutate()}>
          {t("common.save")}
        </button>
        <button type="button" className="btn btn-outline-secondary" onClick={onDone}>
          {t("common.cancel")}
        </button>
      </div>
    </div>
  );
}

function NewTemplateForm({ onDone }: { onDone: () => void }) {
  const { t } = useTranslation();
  const [code, setCode] = useState("");
  const [channel, setChannel] = useState("EMAIL");
  const [bodyTemplate, setBodyTemplate] = useState("");

  const mutation = useMutation({
    mutationFn: () => createNotificationTemplate({ code, channel, locale: "zh-TW", body_template: bodyTemplate }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("admin.saveTemplateError")}</div>
      )}
      <div className="row g-2 align-items-end mb-2">
        <div className="col-md-3">
          <label className="form-label">{t("admin.code")}</label>
          <input className="form-control" placeholder={t("admin.codePlaceholder")} value={code} onChange={(e) => setCode(e.target.value)} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("admin.colChannel")}</label>
          <select className="form-select" value={channel} onChange={(e) => setChannel(e.target.value)}>
            <option value="EMAIL">{t("admin.channelEmail")}</option>
            <option value="SMS">{t("admin.channelSms")}</option>
            <option value="PUSH">{t("admin.channelPush")}</option>
            <option value="IN_APP">{t("admin.channelInApp")}</option>
          </select>
        </div>
      </div>
      <label className="form-label">{t("admin.bodyLabel")} ({"{{variable}}"})</label>
      <textarea className="form-control mb-2" rows={3} value={bodyTemplate} onChange={(e) => setBodyTemplate(e.target.value)} />
      <button type="button" className="btn btn-primary" disabled={mutation.isPending || !code || !bodyTemplate} onClick={() => mutation.mutate()}>
        {t("admin.saveOverride")}
      </button>
    </div>
  );
}
