import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { deleteBimModel, getBimModel, listBimModels, presignUpload, registerBimModel, resetBimModel, type BimModel } from "../../api/spatial";
import { ApiError } from "../../api/client";
import { humanizeEnum } from "../../lib/format";
import { EmptyState } from "../../shell/EmptyState";
import { BimUnresolvedElementsPanel } from "./BimUnresolvedElementsPanel";

const MAX_UPLOAD_BYTES = 1024 * 1024 * 1024;

function statusBadge(status: string | undefined): string {
  switch (status) {
    case "PARSED":
      return "bg-green-lt";
    case "PARSE_FAILED":
      return "bg-red-lt";
    case "PARSING":
      return "bg-yellow-lt";
    default:
      return "bg-blue-lt";
  }
}

export function BimModelsTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [uploadError, setUploadError] = useState<string | null>(null);
  const [uploading, setUploading] = useState(false);
  const [resolvingId, setResolvingId] = useState<string | null>(null);

  const modelsQuery = useQuery({
    queryKey: ["bim-models", facilityId],
    queryFn: () => listBimModels(facilityId),
    refetchInterval: (query) => (query.state.data?.data?.some((m) => m.status === "UPLOADED" || m.status === "PARSING") ? 1500 : false),
  });

  async function handleUpload(file: File) {
    if (!file.name.toLowerCase().endsWith(".ifc")) {
      setUploadError(t("facilities.uploadInvalidFormat"));
      if (fileInputRef.current) fileInputRef.current.value = "";
      return;
    }
    if (file.size > MAX_UPLOAD_BYTES) {
      setUploadError(t("facilities.uploadTooLarge", { limit: Math.round(MAX_UPLOAD_BYTES / 1024 / 1024) }));
      if (fileInputRef.current) fileInputRef.current.value = "";
      return;
    }
    setUploading(true);
    setUploadError(null);
    try {
      const presigned = await presignUpload(file.name, file.type || "application/octet-stream", file.size);
      const putRes = await fetch(presigned.upload_url!, { method: "PUT", body: file, headers: { "Content-Type": presigned.content_type ?? file.type } });
      if (!putRes.ok) throw new Error(t("facilities.storageUploadFailed", { status: putRes.status }));
      await registerBimModel(facilityId, {
        name: file.name,
        source_format: "IFC",
        storage_bucket: "fms-bim",
        storage_key: presigned.storage_key!,
        auto_map: true,
      });
      queryClient.invalidateQueries({ queryKey: ["bim-models", facilityId] });
    } catch (err) {
      setUploadError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.uploadFailedGeneric"));
    } finally {
      setUploading(false);
      if (fileInputRef.current) fileInputRef.current.value = "";
    }
  }

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("facilities.tabBim")}</h3>
        <label className="btn btn-sm btn-primary ms-auto mb-0">
          {uploading ? t("facilities.uploading") : t("facilities.uploadModel")}
          <input
            ref={fileInputRef}
            type="file"
            accept=".ifc"
            hidden
            disabled={uploading}
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (file) void handleUpload(file);
            }}
          />
        </label>
      </div>
      {uploadError && <div className="alert alert-danger m-3">{uploadError}</div>}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("facilities.colName")}</th>
              <th>{t("facilities.colFormat")}</th>
              <th>{t("common.status")}</th>
              <th>{t("facilities.colElements")}</th>
              <th>{t("facilities.colMappedNodesAssets")}</th>
              <th>{t("facilities.colUnresolved")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {modelsQuery.data?.data?.map((m) => (
              <BimModelRow key={m.id} modelId={m.id!} fallback={m} resolving={resolvingId === m.id} onToggleResolve={() => setResolvingId(resolvingId === m.id ? null : m.id!)} facilityId={facilityId} />
            ))}
          </tbody>
        </table>
      </div>
      {modelsQuery.data && modelsQuery.data.data?.length === 0 && <EmptyState title={t("facilities.noBimModels")} subtitle={t("facilities.noBimModelsSubtitle")} />}
    </div>
  );
}

function BimModelRow({
  modelId,
  fallback,
  resolving,
  onToggleResolve,
  facilityId,
}: {
  modelId: string;
  fallback: BimModel;
  resolving: boolean;
  onToggleResolve: () => void;
  facilityId: string;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const { data } = useQuery({
    queryKey: ["bim-model", modelId],
    queryFn: () => getBimModel(modelId),
    refetchInterval: (query) => {
      const status = query.state.data?.data?.status;
      return status === "UPLOADED" || status === "PARSING" ? 1500 : false;
    },
  });
  const m = data?.data ?? fallback;
  const canResolve = m.status === "PARSED" && (m.unresolved_count ?? 0) > 0;
  const canReset = m.status === "PARSED" || m.status === "PARSE_FAILED";
  const canDelete = m.status !== "PARSING";

  async function invalidate() {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["bim-models", facilityId] }),
      queryClient.invalidateQueries({ queryKey: ["bim-model", modelId] }),
    ]);
  }

  async function handleReset() {
    if (!window.confirm(t("facilities.confirmResetBim", { name: m.name }))) return;
    setBusy(true);
    setActionError(null);
    try {
      await resetBimModel(modelId);
      await invalidate();
    } catch (err) {
      setActionError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.bimActionFailedGeneric"));
    } finally {
      setBusy(false);
    }
  }

  async function handleDelete() {
    if (!window.confirm(t("facilities.confirmDeleteBim", { name: m.name }))) return;
    setBusy(true);
    setActionError(null);
    try {
      await deleteBimModel(modelId);
      await invalidate();
    } catch (err) {
      setActionError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.bimActionFailedGeneric"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Fragment>
      <tr>
        <td>{m.name}</td>
        <td className="text-secondary">{m.source_format}</td>
        <td>
          <span className={`badge ${statusBadge(m.status)}`}>{humanizeEnum(m.status)}</span>
        </td>
        <td>{m.element_count ?? "—"}</td>
        <td>
          {m.mapped_node_count ?? 0} / {m.mapped_asset_count ?? 0}
        </td>
        <td>{m.unresolved_count ?? "—"}</td>
        <td>
          <div className="d-flex gap-1 justify-content-end">
            {canResolve && (
              <button type="button" className="btn btn-sm btn-outline-primary" onClick={onToggleResolve} disabled={busy}>
                {resolving ? t("facilities.hide") : t("facilities.resolve")}
              </button>
            )}
            {canReset && (
              <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => void handleReset()} disabled={busy}>
                {t("facilities.resetBim")}
              </button>
            )}
            {canDelete && (
              <button type="button" className="btn btn-sm btn-outline-danger" onClick={() => void handleDelete()} disabled={busy}>
                {t("facilities.deleteBim")}
              </button>
            )}
          </div>
        </td>
      </tr>
      {actionError && (
        <tr>
          <td colSpan={7} className="bg-body-tertiary">
            <div className="alert alert-danger mb-0">{actionError}</div>
          </td>
        </tr>
      )}
      {resolving && (
        <tr>
          <td colSpan={7} className="bg-body-tertiary">
            <BimUnresolvedElementsPanel bimModelId={modelId} facilityId={facilityId} />
          </td>
        </tr>
      )}
    </Fragment>
  );
}
