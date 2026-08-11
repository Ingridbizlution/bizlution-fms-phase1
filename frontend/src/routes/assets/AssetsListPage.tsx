import { useState } from "react";
import { Link } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { listAssets } from "../../api/assets";
import { Can } from "../../auth/Can";
import { useAuth } from "../../auth/AuthContext";
import { assetStatusBadge, criticalityBadge, healthScoreColor } from "../../lib/statusColors";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";
import { AssetBulkImportPanel } from "./AssetBulkImportPanel";

export function AssetsListPage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [q, setQ] = useState("");
  const [status, setStatus] = useState("");
  const [showImport, setShowImport] = useState(false);

  const { items, isLoading, isError, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(
    ["assets", facilityId, q, status],
    (cursor) => listAssets({ facilityId: facilityId ?? undefined, q: q || undefined, status: status || undefined, cursor }),
    { enabled: !!facilityId },
  );

  return (
    <>
      <PageHeader
        title={t("assets.title")}
        actions={
          <div className="d-flex gap-2">
            <Link to="/asset-models" className="btn btn-outline-secondary">
              {t("assets.manageModels")}
            </Link>
            <Can permission="asset:write">
              <button type="button" className="btn btn-outline-primary" onClick={() => setShowImport((s) => !s)}>
                {showImport ? t("assets.cancelImport") : t("assets.bulkImport")}
              </button>
              <Link to="/assets/new" className="btn btn-primary">
                {t("assets.newAsset")}
              </Link>
            </Can>
          </div>
        }
      />
      <PageBody>
        <div className="card">
          {showImport && facilityId && <AssetBulkImportPanel facilityId={facilityId} onImported={() => setShowImport(false)} />}
          <div className="card-header">
            <input
              className="form-control form-control-sm w-auto"
              placeholder={t("assets.searchPlaceholder")}
              value={q}
              onChange={(e) => setQ(e.target.value)}
            />
            <select className="form-select form-select-sm w-auto ms-2" value={status} onChange={(e) => setStatus(e.target.value)}>
              <option value="">{t("assets.allStatuses")}</option>
              <option value="ACTIVE">{t("assets.statusActive")}</option>
              <option value="DOWN">{t("assets.statusDown")}</option>
              <option value="DEGRADED">{t("assets.statusDegraded")}</option>
              <option value="MAINTENANCE">{t("assets.statusMaintenance")}</option>
              <option value="RETIRED">{t("assets.statusRetired")}</option>
            </select>
          </div>
          <div className="table-responsive">
            <table className="table table-vcenter card-table">
              <thead>
                <tr>
                  <th>{t("assets.colCode")}</th>
                  <th>{t("assets.colName")}</th>
                  <th>{t("assets.colCategory")}</th>
                  <th>{t("assets.colStatus")}</th>
                  <th>{t("assets.colCriticality")}</th>
                  <th>{t("assets.colHealth")}</th>
                  <th>{t("assets.colOpenWOs")}</th>
                </tr>
              </thead>
              <tbody>
                {items.map((asset) => (
                  <tr key={asset.id}>
                    <td>
                      <Link to={`/assets/${asset.id}`} className="text-reset">
                        <code>{asset.asset_code}</code>
                      </Link>
                    </td>
                    <td>{asset.name}</td>
                    <td className="text-secondary">{asset.category_code}</td>
                    <td>
                      <span className={`badge ${assetStatusBadge(asset.status)}`}>{asset.status}</span>
                    </td>
                    <td>
                      <span className={`badge ${criticalityBadge(asset.criticality)}`}>{asset.criticality}</span>
                    </td>
                    <td className={healthScoreColor(asset.health_score)}>{asset.health_score != null ? asset.health_score.toFixed(0) : "—"}</td>
                    <td>{asset.open_work_order_count ?? 0}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {!isLoading && items.length === 0 && <EmptyState title={t("assets.noAssetsMatch")} />}
          {isError && <div className="alert alert-danger m-3">{t("assets.loadError")}</div>}
          <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
        </div>
      </PageBody>
    </>
  );
}
