import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { listAssets, type Asset } from "../../api/assets";
import { ApiError } from "../../api/client";
import { createBimMappings, listSpatialNodes, listUnresolvedBimElements, type SpatialNode } from "../../api/spatial";

function MappingRow({ bimModelId, facilityId, elementId, name, elementType, onMapped }: { bimModelId: string; facilityId: string; elementId: string; name?: string; elementType?: string; onMapped: () => void }) {
  const { t } = useTranslation();
  const [targetType, setTargetType] = useState<"SPATIAL_NODE" | "ASSET">("SPATIAL_NODE");
  const [targetId, setTargetId] = useState("");

  const nodesQuery = useQuery({ queryKey: ["spatial-nodes", facilityId], queryFn: () => listSpatialNodes(facilityId), enabled: targetType === "SPATIAL_NODE" });
  const assetsQuery = useQuery({ queryKey: ["assets-picker", facilityId], queryFn: () => listAssets({ facilityId, limit: 50 }), enabled: targetType === "ASSET" });

  const mutation = useMutation({
    mutationFn: () => createBimMappings(bimModelId, [{ bim_element_id: elementId, target_type: targetType, target_id: targetId }]),
    onSuccess: onMapped,
  });

  const options = targetType === "SPATIAL_NODE" ? nodesQuery.data?.data ?? [] : assetsQuery.data?.data ?? [];

  return (
    <tr>
      <td>
        <code>{elementType}</code>
        <div>{name}</div>
      </td>
      <td>
        <select className="form-select form-select-sm" value={targetType} onChange={(e) => setTargetType(e.target.value as "SPATIAL_NODE" | "ASSET")}>
          <option value="SPATIAL_NODE">{t("facilities.targetSpatialNode")}</option>
          <option value="ASSET">{t("facilities.targetAsset")}</option>
        </select>
      </td>
      <td>
        <select className="form-select form-select-sm" value={targetId} onChange={(e) => setTargetId(e.target.value)}>
          <option value="">{t("facilities.selectPlaceholder")}</option>
          {options.map((o) => (
            <option value={o.id} key={o.id}>
              {"asset_code" in o ? `${(o as Asset).asset_code} — ${o.name}` : `${(o as SpatialNode).node_path ?? o.name}`}
            </option>
          ))}
        </select>
      </td>
      <td>
        <button type="button" className="btn btn-sm btn-primary" disabled={!targetId || mutation.isPending} onClick={() => mutation.mutate()}>
          {t("facilities.map")}
        </button>
        {mutation.isError && <div className="text-danger small">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("facilities.mapFailed")}</div>}
      </td>
    </tr>
  );
}

export function BimUnresolvedElementsPanel({ bimModelId, facilityId }: { bimModelId: string; facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data, isLoading } = useQuery({ queryKey: ["bim-unresolved", bimModelId], queryFn: () => listUnresolvedBimElements(bimModelId) });

  function refresh() {
    queryClient.invalidateQueries({ queryKey: ["bim-unresolved", bimModelId] });
    queryClient.invalidateQueries({ queryKey: ["bim-model", bimModelId] });
    queryClient.invalidateQueries({ queryKey: ["bim-models", facilityId] });
  }

  if (isLoading) return <div className="text-secondary small py-2">{t("facilities.loadingUnresolvedElements")}</div>;
  if (!data?.data?.length) return <div className="text-secondary small py-2">{t("facilities.nothingLeftToResolve")}</div>;

  return (
    <table className="table table-sm mb-0">
      <thead>
        <tr>
          <th>{t("facilities.colElement")}</th>
          <th>{t("facilities.colTargetType")}</th>
          <th>{t("facilities.colTarget")}</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {data.data.map((el) => (
          <MappingRow key={el.bim_element_id} bimModelId={bimModelId} facilityId={facilityId} elementId={el.bim_element_id!} name={el.name} elementType={el.element_type} onMapped={refresh} />
        ))}
      </tbody>
    </table>
  );
}
