import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { listWebhooks, upsertWebhook, type WebhookSubscription } from "../../api/admin";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

const EVENT_TYPES = ["work_order.created", "work_order.status_changed", "reservation.confirmed", "reservation.cancelled", "alarm.raised"];

export function WebhooksTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [newSecret, setNewSecret] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const query = useQuery({ queryKey: ["webhooks"], queryFn: listWebhooks });

  const toggleMutation = useMutation({
    mutationFn: (wh: WebhookSubscription) =>
      upsertWebhook({ url: wh.url!, event_types: wh.event_types ?? [], description: wh.description ?? undefined, is_active: !wh.is_active }),
    onSuccess: () => {
      setRowError(null);
      queryClient.invalidateQueries({ queryKey: ["webhooks"] });
    },
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.saveSubscriptionError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("admin.webhookSubscriptions")}</h3>
        <Can permission="tenant:update">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("admin.newSubscription")}
          </button>
        </Can>
      </div>
      {newSecret && (
        <div className="alert alert-warning m-3 mb-0">
          {t("admin.signingSecretHint")} <code>{newSecret}</code>
        </div>
      )}
      {showForm && (
        <NewWebhookForm
          onDone={(secret) => {
            setShowForm(false);
            if (secret) setNewSecret(secret);
            queryClient.invalidateQueries({ queryKey: ["webhooks"] });
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
              <th>{t("admin.colUrl")}</th>
              <th>{t("admin.colEvents")}</th>
              <th>{t("common.status")}</th>
              <th>{t("admin.colLastSuccess")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {query.data?.data.map((wh) => (
              <tr key={wh.id}>
                <td>
                  <code>{wh.url}</code>
                </td>
                <td className="text-secondary">{wh.event_types?.join(", ")}</td>
                <td>
                  {wh.is_active ? <span className="badge bg-green-lt">{t("admin.active")}</span> : <span className="badge bg-red-lt">{t("admin.disabled")}</span>}
                  {(wh.consecutive_failures ?? 0) > 0 && <span className="badge bg-yellow-lt ms-1">{t("admin.failuresCount", { count: wh.consecutive_failures })}</span>}
                </td>
                <td className="text-secondary">{wh.last_success_at ? new Date(wh.last_success_at).toLocaleString() : t("admin.never")}</td>
                <td className="text-end">
                  <Can permission="tenant:update">
                    <button
                      type="button"
                      className={`btn btn-sm ${wh.is_active ? "btn-outline-danger" : "btn-outline-success"}`}
                      disabled={toggleMutation.isPending}
                      onClick={() => toggleMutation.mutate(wh)}
                    >
                      {wh.is_active ? t("admin.disableSubscription") : t("admin.enableSubscription")}
                    </button>
                  </Can>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!query.isLoading && query.data?.data.length === 0 && <EmptyState title={t("admin.noWebhookSubscriptions")} />}
    </div>
  );
}

function NewWebhookForm({ onDone }: { onDone: (secret?: string) => void }) {
  const { t } = useTranslation();
  const [url, setUrl] = useState("");
  const [events, setEvents] = useState<Set<string>>(new Set());

  const mutation = useMutation({
    mutationFn: () => upsertWebhook({ url, event_types: [...events] }),
    onSuccess: (res) => onDone(res.signing_secret),
  });

  function toggle(evt: string) {
    setEvents((prev) => {
      const next = new Set(prev);
      if (next.has(evt)) next.delete(evt);
      else next.add(evt);
      return next;
    });
  }

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("admin.saveSubscriptionError")}</div>
      )}
      <div className="mb-2">
        <label className="form-label">{t("admin.urlHttpsOnly")}</label>
        <input className="form-control" placeholder="https://example.com/webhooks/fms" value={url} onChange={(e) => setUrl(e.target.value)} />
      </div>
      <label className="form-label">{t("admin.colEvents")}</label>
      <div className="d-flex flex-wrap gap-2 mb-2">
        {EVENT_TYPES.map((evt) => (
          <label className="form-check" key={evt}>
            <input type="checkbox" className="form-check-input" checked={events.has(evt)} onChange={() => toggle(evt)} />
            <span className="form-check-label">
              <code>{evt}</code>
            </span>
          </label>
        ))}
      </div>
      <button type="button" className="btn btn-primary" disabled={mutation.isPending || !url || events.size === 0} onClick={() => mutation.mutate()}>
        {t("admin.saveSubscription")}
      </button>
    </div>
  );
}
