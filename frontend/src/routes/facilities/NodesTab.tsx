import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { createSpatialNode, deleteSpatialNode, listSpatialNodeTypes, listSpatialNodes, updateSpatialNode, type SpatialNode } from "../../api/spatial";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";

export function NodesTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const { items, isLoading, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(["spatial-nodes", facilityId], (cursor) =>
    listSpatialNodes(facilityId, { cursor }),
  );

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["spatial-nodes", facilityId] });
  }

  const deleteMutation = useMutation({
    mutationFn: (nodeId: string) => deleteSpatialNode(nodeId),
    onSuccess: (res) => {
      void invalidate();
      const assets = res.meta?.assets_still_referencing ?? 0;
      const plans = res.meta?.maintenance_plans_still_referencing ?? 0;
      if (assets > 0 || plans > 0) window.alert(t("facilities.nodeDeletedStillReferenced", { assets, plans }));
    },
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.nodeDeleteError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("facilities.tabNodes")}</h3>
        <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
          {showForm ? t("common.cancel") : t("facilities.newNode")}
        </button>
      </div>
      {showForm && <NewNodeForm facilityId={facilityId} onDone={() => setShowForm(false)} />}
      {rowError && (
        <div className="alert alert-danger m-3 mb-0" onClick={() => setRowError(null)}>
          {rowError}
        </div>
      )}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("facilities.colPath")}</th>
              <th>{t("facilities.colName")}</th>
              <th>{t("facilities.colType")}</th>
              <th>{t("facilities.colFloor")}</th>
              <th>{t("facilities.colBookable")}</th>
              <th>{t("facilities.colAssets")}</th>
              <th>{t("facilities.colOpenWOs")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((n) => (
              <Fragment key={n.id}>
                <tr>
                  <td>
                    <code>{n.node_path}</code>
                  </td>
                  <td>{n.name}</td>
                  <td className="text-secondary">{n.node_type_code}</td>
                  <td>{n.floor_label ?? n.floor_level ?? "—"}</td>
                  <td>{n.is_bookable ? <span className="badge bg-blue-lt">{t("facilities.bookableBadge")}</span> : "—"}</td>
                  <td>{n.asset_count ?? 0}</td>
                  <td>{n.open_work_order_count ?? 0}</td>
                  <td className="text-end">
                    {n.bim_element_id ? (
                      <span className="badge bg-secondary-lt" title={t("facilities.bimImportedHint")}>
                        {t("facilities.bimImportedBadge")}
                      </span>
                    ) : (
                      <Can permission="spatial_node:write">
                        <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === n.id ? null : (n.id ?? null))}>
                          {t("common.edit")}
                        </button>
                        <button
                          type="button"
                          className="btn btn-sm btn-outline-danger"
                          disabled={deleteMutation.isPending}
                          onClick={() => {
                            if (window.confirm(t("facilities.confirmDeleteNode", { name: n.name }))) deleteMutation.mutate(n.id!);
                          }}
                        >
                          {t("common.delete")}
                        </button>
                      </Can>
                    )}
                  </td>
                </tr>
                {editingId === n.id && (
                  <tr>
                    <td colSpan={8} className="bg-body-tertiary">
                      <EditNodeForm
                        node={n}
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
      {!isLoading && items.length === 0 && <EmptyState title={t("facilities.noSpatialNodes")} />}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}

function NewNodeForm({ facilityId, onDone }: { facilityId: string; onDone: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [nodeTypeCode, setNodeTypeCode] = useState("");
  const [isBookable, setIsBookable] = useState(false);

  const typesQuery = useQuery({ queryKey: ["spatial-node-types"], queryFn: listSpatialNodeTypes });

  const mutation = useMutation({
    mutationFn: () => createSpatialNode(facilityId, { code, name, node_type_code: nodeTypeCode, is_bookable: isBookable }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["spatial-nodes", facilityId] });
      onDone();
    },
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("facilities.createNodeError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-3">
          <label className="form-label">{t("facilities.code")}</label>
          <input className="form-control" value={code} onChange={(e) => setCode(e.target.value)} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("common.type")}</label>
          <select className="form-select" value={nodeTypeCode} onChange={(e) => setNodeTypeCode(e.target.value)}>
            <option value="">{t("facilities.selectPlaceholder")}</option>
            {typesQuery.data?.data?.map((nodeType) => (
              <option value={nodeType.code} key={nodeType.code}>
                {nodeType.name}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-check">
            <input type="checkbox" className="form-check-input" checked={isBookable} onChange={(e) => setIsBookable(e.target.checked)} />
            <span className="form-check-label">{t("facilities.bookable")}</span>
          </label>
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending || !code || !name || !nodeTypeCode} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}

function EditNodeForm({ node, onDone }: { node: SpatialNode; onDone: () => void }) {
  const { t } = useTranslation();
  const [name, setName] = useState(node.name ?? "");
  const [nodeTypeCode, setNodeTypeCode] = useState(node.node_type_code ?? "");
  const [floorLabel, setFloorLabel] = useState(node.floor_label ?? "");
  const [isBookable, setIsBookable] = useState(node.is_bookable ?? false);

  const typesQuery = useQuery({ queryKey: ["spatial-node-types"], queryFn: listSpatialNodeTypes });

  const mutation = useMutation({
    mutationFn: () => updateSpatialNode(node.id!, { name, node_type_code: nodeTypeCode, floor_label: floorLabel || null, is_bookable: isBookable }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("facilities.nodeSaveError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-3">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("common.type")}</label>
          <select className="form-select" value={nodeTypeCode} onChange={(e) => setNodeTypeCode(e.target.value)}>
            {typesQuery.data?.data?.map((nodeType) => (
              <option value={nodeType.code} key={nodeType.code}>
                {nodeType.name}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.colFloor")}</label>
          <input className="form-control" value={floorLabel} onChange={(e) => setFloorLabel(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-check">
            <input type="checkbox" className="form-check-input" checked={isBookable} onChange={(e) => setIsBookable(e.target.checked)} />
            <span className="form-check-label">{t("facilities.bookable")}</span>
          </label>
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending || !name} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-outline-secondary w-100" onClick={onDone}>
            {t("common.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}
