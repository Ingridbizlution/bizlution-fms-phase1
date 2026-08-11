import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { addWorkOrderComment, getWorkOrder, listAvailableActions, transitionWorkOrder, updateWorkOrder, updateWorkOrderTask, type WorkOrderDetail } from "../../api/workOrders";
import { deleteAttachment, listAttachments, uploadAttachment } from "../../api/attachments";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { humanizeEnum } from "../../lib/format";
import { priorityBadge, workOrderCategoryBadge } from "../../lib/statusColors";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

/** Turns a local datetime input value into an RFC 3339 string with an explicit offset —
 *  the backend deserializes scheduled_*_at as `chrono::DateTime<Utc>` and rejects a bare local-looking string. */
function toRfc3339(localDateTime: string): string {
  return new Date(localDateTime).toISOString();
}

function toLocalInputValue(iso: string | undefined | null): string {
  if (!iso) return "";
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function WorkOrderDetailPage() {
  const { t } = useTranslation();
  const { workOrderId } = useParams<{ workOrderId: string }>();
  const queryClient = useQueryClient();
  const [comment, setComment] = useState("");
  const [transitionError, setTransitionError] = useState<string | null>(null);
  const [isEditing, setIsEditing] = useState(false);
  const [editError, setEditError] = useState<string | null>(null);
  const [taskError, setTaskError] = useState<string | null>(null);

  const detailQuery = useQuery({
    queryKey: ["work-order", workOrderId],
    queryFn: () => getWorkOrder(workOrderId!, "tasks,comments,transitions"),
    enabled: !!workOrderId,
  });

  const actionsQuery = useQuery({
    queryKey: ["work-order-actions", workOrderId],
    queryFn: () => listAvailableActions(workOrderId!),
    enabled: !!workOrderId,
  });

  const transitionMutation = useMutation({
    mutationFn: (action: string) => transitionWorkOrder(workOrderId!, detailQuery.data!.version!, { action }),
    onSuccess: () => {
      setTransitionError(null);
      queryClient.invalidateQueries({ queryKey: ["work-order", workOrderId] });
      queryClient.invalidateQueries({ queryKey: ["work-order-actions", workOrderId] });
    },
    onError: (err) => {
      setTransitionError(err instanceof ApiError ? err.problem.detail ?? err.message : t("workOrders.detail.actionFailed"));
    },
  });

  const updateMutation = useMutation({
    mutationFn: (patch: Parameters<typeof updateWorkOrder>[2]) => updateWorkOrder(workOrderId!, detailQuery.data!.version!, patch),
    onSuccess: () => {
      setEditError(null);
      setIsEditing(false);
      queryClient.invalidateQueries({ queryKey: ["work-order", workOrderId] });
    },
    onError: (err) => {
      setEditError(err instanceof ApiError ? err.problem.detail ?? err.message : t("workOrders.detail.editForm.saveError"));
    },
  });

  const taskMutation = useMutation({
    mutationFn: ({ taskId, resultValue }: { taskId: string; resultValue: boolean }) => updateWorkOrderTask(workOrderId!, taskId, { result_value: resultValue }),
    onSuccess: () => {
      setTaskError(null);
      queryClient.invalidateQueries({ queryKey: ["work-order", workOrderId] });
    },
    onError: (err) => {
      setTaskError(err instanceof ApiError ? err.problem.detail ?? err.message : t("workOrders.detail.taskUpdateError"));
    },
  });

  const commentMutation = useMutation({
    mutationFn: () => addWorkOrderComment(workOrderId!, comment),
    onSuccess: () => {
      setComment("");
      queryClient.invalidateQueries({ queryKey: ["work-order", workOrderId] });
    },
  });

  const wo = detailQuery.data;

  if (detailQuery.isLoading) {
    return (
      <PageBody>
        <div className="d-flex justify-content-center py-5">
          <div className="spinner-border text-primary" role="status" aria-label={t("workOrders.detail.loadingWorkOrder")} />
        </div>
      </PageBody>
    );
  }

  if (detailQuery.isError || !wo) {
    return (
      <PageBody>
        <div className="alert alert-danger">{t("workOrders.detail.loadError")}</div>
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader
        pretitle={wo.wo_no}
        title={wo.title ?? t("workOrders.detail.defaultTitle")}
        actions={
          <div className="d-flex gap-2">
            {actionsQuery.data?.data
              ?.filter((a) => a.permitted !== false)
              .map((action) => (
                <button
                  key={action.action}
                  type="button"
                  className="btn btn-outline-primary"
                  disabled={transitionMutation.isPending}
                  onClick={() => transitionMutation.mutate(action.action!)}
                >
                  {action.label_zh ?? action.action}
                </button>
              ))}
          </div>
        }
      />
      <PageBody>
        {transitionError && <div className="alert alert-danger">{transitionError}</div>}

        <div className="row row-deck row-cards g-3">
          <div className="col-md-6">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("workOrders.detail.details")}</h3>
                <span className={`badge ${workOrderCategoryBadge(wo.status_category)} ms-2`}>{humanizeEnum(wo.status)}</span>
                <span className={`badge ${priorityBadge(wo.priority)} ms-1`}>{wo.priority}</span>
                {!isEditing && (
                  <Can permission="work_order:update">
                    <button type="button" className="btn btn-sm btn-outline-secondary ms-auto" onClick={() => setIsEditing(true)}>
                      {t("common.edit")}
                    </button>
                  </Can>
                )}
              </div>
              <div className="card-body">
                {isEditing ? (
                  <WorkOrderEditForm
                    wo={wo}
                    error={editError}
                    saving={updateMutation.isPending}
                    onCancel={() => {
                      setIsEditing(false);
                      setEditError(null);
                    }}
                    onSave={(patch) => updateMutation.mutate(patch)}
                  />
                ) : (
                  <>
                    <p>{wo.description ?? <span className="text-secondary">{t("workOrders.detail.noDescription")}</span>}</p>
                    <dl className="row mb-0">
                      <dt className="col-5">{t("workOrders.detail.type")}</dt>
                      <dd className="col-7">{wo.work_order_type}</dd>
                      <dt className="col-5">{t("workOrders.detail.asset")}</dt>
                      <dd className="col-7">{wo.asset?.name ?? "—"}</dd>
                      <dt className="col-5">{t("workOrders.detail.location")}</dt>
                      <dd className="col-7">{wo.location?.node_path ?? "—"}</dd>
                      <dt className="col-5">{t("workOrders.detail.requester")}</dt>
                      <dd className="col-7">{wo.requester?.display_name ?? "—"}</dd>
                      <dt className="col-5">{t("workOrders.detail.assignee")}</dt>
                      <dd className="col-7">{wo.assignee?.display_name ?? t("workOrders.detail.unassigned")}</dd>
                      <dt className="col-5">{t("workOrders.detail.slaState")}</dt>
                      <dd className="col-7">{humanizeEnum(wo.sla_state) || "—"}</dd>
                      <dt className="col-5">{t("workOrders.detail.scheduledStart")}</dt>
                      <dd className="col-7">{wo.scheduled_start_at ? new Date(wo.scheduled_start_at).toLocaleString() : "—"}</dd>
                      <dt className="col-5">{t("workOrders.detail.scheduledEnd")}</dt>
                      <dd className="col-7">{wo.scheduled_end_at ? new Date(wo.scheduled_end_at).toLocaleString() : "—"}</dd>
                    </dl>
                  </>
                )}
              </div>
            </div>
          </div>

          <div className="col-md-6">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("workOrders.detail.checklist")}</h3>
              </div>
              <div className="card-body">
                {taskError && <div className="alert alert-danger">{taskError}</div>}
                {wo.tasks?.length ? (
                  <ul className="list-unstyled mb-0">
                    {wo.tasks.map((task) => (
                      <li key={task.id} className="mb-1 d-flex align-items-center">
                        {task.input_type === "CHECKBOX" ? (
                          <Can
                            permission="work_order:execute"
                            fallback={
                              <span className={`badge ${task.completed_at ? "bg-green-lt" : "bg-secondary-lt"} me-2`}>
                                {task.completed_at ? t("workOrders.detail.done") : t("workOrders.detail.pending")}
                              </span>
                            }
                          >
                            <input
                              type="checkbox"
                              className="form-check-input me-2"
                              checked={task.result_value === true}
                              disabled={taskMutation.isPending}
                              onChange={(e) => taskMutation.mutate({ taskId: task.id!, resultValue: e.target.checked })}
                            />
                          </Can>
                        ) : (
                          <span className={`badge ${task.completed_at ? "bg-green-lt" : "bg-secondary-lt"} me-2`}>
                            {task.completed_at ? t("workOrders.detail.done") : t("workOrders.detail.pending")}
                          </span>
                        )}
                        {task.title}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="text-secondary mb-0">{t("workOrders.detail.noChecklist")}</p>
                )}
              </div>
            </div>
          </div>

          <div className="col-12">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("workOrders.detail.comments")}</h3>
              </div>
              <div className="card-body">
                <div className="list-group list-group-flush mb-3">
                  {wo.comments?.length ? (
                    wo.comments.map((c) => (
                      <div className="list-group-item" key={c.id}>
                        <div className="d-flex justify-content-between">
                          <strong>{c.author_name}</strong>
                          <span className="text-secondary small">{c.created_at ? new Date(c.created_at).toLocaleString() : ""}</span>
                        </div>
                        <div>{c.body}</div>
                      </div>
                    ))
                  ) : (
                    <p className="text-secondary mb-0">{t("workOrders.detail.noComments")}</p>
                  )}
                </div>
                <form
                  className="input-group"
                  onSubmit={(e) => {
                    e.preventDefault();
                    if (comment.trim()) commentMutation.mutate();
                  }}
                >
                  <input className="form-control" placeholder={t("workOrders.detail.addCommentPlaceholder")} value={comment} onChange={(e) => setComment(e.target.value)} />
                  <button type="submit" className="btn btn-outline-primary" disabled={commentMutation.isPending || !comment.trim()}>
                    {t("workOrders.detail.post")}
                  </button>
                </form>
              </div>
            </div>
          </div>

          <div className="col-12">
            <AttachmentsCard workOrderId={workOrderId!} />
          </div>
        </div>
      </PageBody>
    </>
  );
}

function AttachmentsCard({ workOrderId }: { workOrderId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [purpose, setPurpose] = useState("GENERAL");
  const [uploadError, setUploadError] = useState<string | null>(null);

  const query = useQuery({ queryKey: ["work-order-attachments", workOrderId], queryFn: () => listAttachments("WORK_ORDER", workOrderId) });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["work-order-attachments", workOrderId] });
  }

  const uploadMutation = useMutation({
    mutationFn: (file: File) => uploadAttachment("WORK_ORDER", workOrderId, file, purpose),
    onSuccess: () => {
      setUploadError(null);
      void invalidate();
    },
    onError: (err) => setUploadError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("workOrders.detail.attachmentUploadError")),
  });

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteAttachment(id),
    onSuccess: invalidate,
    onError: (err) => setUploadError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("workOrders.detail.attachmentDeleteError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("workOrders.detail.attachments")}</h3>
      </div>
      <div className="card-body">
        {uploadError && <div className="alert alert-danger">{uploadError}</div>}
        <div className="list-group list-group-flush mb-3">
          {query.data?.data?.length ? (
            query.data.data.map((att) => (
              <div className="list-group-item d-flex justify-content-between align-items-center" key={att.id}>
                <div>
                  <a href={att.download_url} target="_blank" rel="noreferrer">
                    {att.file_name}
                  </a>
                  <div className="text-secondary small">
                    {att.purpose} · {att.size_bytes != null ? `${Math.round(att.size_bytes / 1024)} KB` : "—"}
                  </div>
                </div>
                <Can permission="work_order:update">
                  <button
                    type="button"
                    className="btn btn-sm btn-outline-danger"
                    disabled={deleteMutation.isPending}
                    onClick={() => {
                      if (window.confirm(t("workOrders.detail.confirmDeleteAttachment", { name: att.file_name }))) deleteMutation.mutate(att.id!);
                    }}
                  >
                    {t("common.delete")}
                  </button>
                </Can>
              </div>
            ))
          ) : (
            <p className="text-secondary mb-0">{t("workOrders.detail.noAttachments")}</p>
          )}
        </div>
        <Can permission="work_order:update">
          <div className="row g-2 align-items-end">
            <div className="col-auto">
              <label className="form-label">{t("workOrders.detail.attachmentPurpose")}</label>
              <select className="form-select" value={purpose} onChange={(e) => setPurpose(e.target.value)}>
                <option value="GENERAL">{t("workOrders.detail.purposeGeneral")}</option>
                <option value="BEFORE_PHOTO">{t("workOrders.detail.purposeBeforePhoto")}</option>
                <option value="AFTER_PHOTO">{t("workOrders.detail.purposeAfterPhoto")}</option>
                <option value="MANUAL">{t("workOrders.detail.purposeManual")}</option>
                <option value="SIGNATURE">{t("workOrders.detail.purposeSignature")}</option>
              </select>
            </div>
            <div className="col-auto">
              <input
                type="file"
                className="form-control"
                disabled={uploadMutation.isPending}
                onChange={(e) => {
                  const file = e.target.files?.[0];
                  if (file) uploadMutation.mutate(file);
                  e.target.value = "";
                }}
              />
            </div>
          </div>
        </Can>
      </div>
    </div>
  );
}

function WorkOrderEditForm({
  wo,
  error,
  saving,
  onCancel,
  onSave,
}: {
  wo: WorkOrderDetail;
  error: string | null;
  saving: boolean;
  onCancel: () => void;
  onSave: (patch: { title: string; description?: string; priority: string; scheduled_start_at?: string; scheduled_end_at?: string }) => void;
}) {
  const { t } = useTranslation();
  const [form, setForm] = useState({
    title: wo.title ?? "",
    description: wo.description ?? "",
    priority: wo.priority ?? "MEDIUM",
    scheduled_start_at: toLocalInputValue(wo.scheduled_start_at),
    scheduled_end_at: toLocalInputValue(wo.scheduled_end_at),
  });

  return (
    <div>
      {error && <div className="alert alert-danger">{error}</div>}
      <div className="mb-3">
        <label className="form-label">{t("workOrders.create.titleLabel")}</label>
        <input className="form-control" value={form.title} onChange={(e) => setForm((f) => ({ ...f, title: e.target.value }))} />
      </div>
      <div className="mb-3">
        <label className="form-label">{t("workOrders.create.description")}</label>
        <textarea className="form-control" rows={3} value={form.description} onChange={(e) => setForm((f) => ({ ...f, description: e.target.value }))} />
      </div>
      <div className="row g-2 mb-3">
        <div className="col-md-4">
          <label className="form-label">{t("workOrders.create.priority")}</label>
          <select className="form-select" value={form.priority} onChange={(e) => setForm((f) => ({ ...f, priority: e.target.value }))}>
            <option value="LOW">{t("workOrders.create.priorityLow")}</option>
            <option value="MEDIUM">{t("workOrders.create.priorityMedium")}</option>
            <option value="HIGH">{t("workOrders.create.priorityHigh")}</option>
            <option value="URGENT">{t("workOrders.create.priorityUrgent")}</option>
            <option value="CRITICAL">{t("workOrders.create.priorityCritical")}</option>
          </select>
        </div>
        <div className="col-md-4">
          <label className="form-label">{t("workOrders.detail.scheduledStart")}</label>
          <input type="datetime-local" className="form-control" value={form.scheduled_start_at} onChange={(e) => setForm((f) => ({ ...f, scheduled_start_at: e.target.value }))} />
        </div>
        <div className="col-md-4">
          <label className="form-label">{t("workOrders.detail.scheduledEnd")}</label>
          <input type="datetime-local" className="form-control" value={form.scheduled_end_at} onChange={(e) => setForm((f) => ({ ...f, scheduled_end_at: e.target.value }))} />
        </div>
      </div>
      <div className="d-flex gap-2">
        <button
          type="button"
          className="btn btn-primary"
          disabled={saving || !form.title}
          onClick={() =>
            onSave({
              title: form.title,
              description: form.description || undefined,
              priority: form.priority,
              scheduled_start_at: form.scheduled_start_at ? toRfc3339(form.scheduled_start_at) : undefined,
              scheduled_end_at: form.scheduled_end_at ? toRfc3339(form.scheduled_end_at) : undefined,
            })
          }
        >
          {t("common.save")}
        </button>
        <button type="button" className="btn btn-link" onClick={onCancel}>
          {t("common.cancel")}
        </button>
      </div>
    </div>
  );
}
