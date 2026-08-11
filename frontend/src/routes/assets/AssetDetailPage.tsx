import { useQuery } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { getAsset } from "../../api/assets";
import { humanizeEnum } from "../../lib/format";
import { assetStatusBadge, criticalityBadge, healthScoreColor, priorityBadge, workOrderCategoryBadge } from "../../lib/statusColors";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

export function AssetDetailPage() {
  const { t } = useTranslation();
  const { assetId } = useParams<{ assetId: string }>();
  const { data, isLoading, isError } = useQuery({
    queryKey: ["asset", assetId],
    queryFn: () => getAsset(assetId!, "children,relations,meters,open_work_orders,maintenance_plans"),
    enabled: !!assetId,
  });

  if (isLoading) {
    return (
      <PageBody>
        <div className="d-flex justify-content-center py-5">
          <div className="spinner-border text-primary" role="status" aria-label={t("assets.detail.loadingAsset")} />
        </div>
      </PageBody>
    );
  }

  if (isError || !data) {
    return (
      <PageBody>
        <div className="alert alert-danger">{t("assets.detail.loadError")}</div>
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader pretitle={data.category_code} title={data.name ?? data.asset_code ?? t("assets.detail.defaultTitle")} />
      <PageBody>
        <div className="row row-deck row-cards g-3">
          <div className="col-md-6">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("assets.detail.overview")}</h3>
              </div>
              <div className="card-body">
                <dl className="row mb-0">
                  <dt className="col-5">{t("assets.detail.code")}</dt>
                  <dd className="col-7">
                    <code>{data.asset_code}</code>
                  </dd>
                  <dt className="col-5">{t("assets.detail.serialNo")}</dt>
                  <dd className="col-7">{data.serial_no ?? "—"}</dd>
                  <dt className="col-5">{t("assets.detail.status")}</dt>
                  <dd className="col-7">
                    <span className={`badge ${assetStatusBadge(data.status)}`}>{data.status}</span>
                  </dd>
                  <dt className="col-5">{t("assets.detail.criticality")}</dt>
                  <dd className="col-7">
                    <span className={`badge ${criticalityBadge(data.criticality)}`}>{data.criticality}</span>
                  </dd>
                  <dt className="col-5">{t("assets.detail.healthScore")}</dt>
                  <dd className={`col-7 ${healthScoreColor(data.health_score)}`}>{data.health_score != null ? data.health_score.toFixed(0) : "—"}</dd>
                  <dt className="col-5">{t("assets.detail.installDate")}</dt>
                  <dd className="col-7">{data.install_date ?? "—"}</dd>
                  <dt className="col-5">{t("assets.detail.warrantyEnds")}</dt>
                  <dd className="col-7">{data.warranty_end_date ?? "—"}</dd>
                  <dt className="col-5">{t("assets.detail.location")}</dt>
                  <dd className="col-7">{data.spatial_node_path ?? "—"}</dd>
                </dl>
              </div>
            </div>
          </div>

          <div className="col-md-6">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("assets.detail.meters")}</h3>
              </div>
              <div className="card-body">
                {data.meters?.length ? (
                  <table className="table table-sm mb-0">
                    <thead>
                      <tr>
                        <th>{t("assets.detail.colMeter")}</th>
                        <th>{t("assets.detail.colLastValue")}</th>
                        <th>{t("assets.detail.colReadAt")}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {data.meters.map((m) => (
                        <tr key={m.meter_code}>
                          <td>{m.name}</td>
                          <td>
                            {m.last_value ?? "—"} {m.unit}
                          </td>
                          <td className="text-secondary">{m.last_read_at ? new Date(m.last_read_at).toLocaleString() : "—"}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                ) : (
                  <p className="text-secondary mb-0">{t("assets.detail.noMeters")}</p>
                )}
              </div>
            </div>
          </div>

          <div className="col-md-6">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("assets.detail.openWorkOrders")}</h3>
              </div>
              <div className="card-body">
                {data.open_work_orders?.length ? (
                  <div className="list-group list-group-flush">
                    {data.open_work_orders.map((wo) => (
                      <Link to={`/work-orders/${wo.id}`} key={wo.id} className="list-group-item list-group-item-action d-flex justify-content-between">
                        <span>
                          <code className="me-2">{wo.wo_no}</code>
                          {wo.title}
                        </span>
                        <span className={`badge ${workOrderCategoryBadge(wo.status_category)}`}>{humanizeEnum(wo.status)}</span>
                      </Link>
                    ))}
                  </div>
                ) : (
                  <p className="text-secondary mb-0">{t("assets.detail.noOpenWorkOrders")}</p>
                )}
              </div>
            </div>
          </div>

          <div className="col-md-6">
            <div className="card">
              <div className="card-header">
                <h3 className="card-title">{t("assets.detail.dependencies")}</h3>
              </div>
              <div className="card-body">
                {data.relations?.length ? (
                  <ul className="list-unstyled mb-0">
                    {data.relations.map((rel, i) => (
                      <li key={i} className="mb-1">
                        <span className={`badge ${rel.direction === "upstream" ? "bg-blue-lt" : "bg-purple-lt"} me-2`}>{rel.direction}</span>
                        {rel.asset?.name} <span className={`badge ${priorityBadge(rel.impact_level)} ms-1`}>{rel.impact_level}</span>
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="text-secondary mb-0">{t("assets.detail.noDependencies")}</p>
                )}
              </div>
            </div>
          </div>
        </div>
      </PageBody>
    </>
  );
}
