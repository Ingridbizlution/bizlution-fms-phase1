import { useTranslation } from "react-i18next";
import { listAuditLog } from "../../api/admin";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";

export function AuditLogTab() {
  const { t } = useTranslation();
  const { items, isLoading, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(["audit-log"], (cursor) => listAuditLog(cursor));

  return (
    <div className="card">
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("admin.colWhen")}</th>
              <th>{t("admin.colActor")}</th>
              <th>{t("admin.colAction")}</th>
              <th>{t("admin.colEntity")}</th>
              <th>{t("admin.colRequestId")}</th>
            </tr>
          </thead>
          <tbody>
            {items.map((entry) => (
              <tr key={entry.id}>
                <td className="text-secondary">{entry.occurred_at ? new Date(entry.occurred_at).toLocaleString() : "—"}</td>
                <td>{entry.actor_name ?? <span className="text-secondary">{t("admin.system")}</span>}</td>
                <td>
                  <code>{entry.action}</code>
                </td>
                <td className="text-secondary">
                  {entry.entity_type}
                  {entry.diff_keys?.length ? ` (${entry.diff_keys.join(", ")})` : ""}
                </td>
                <td>
                  <code className="small">{entry.request_id?.slice(0, 8)}</code>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && items.length === 0 && <EmptyState title={t("admin.noAuditEntries")} />}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}
