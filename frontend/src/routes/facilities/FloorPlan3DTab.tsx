import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Component, Suspense, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Canvas, useLoader } from "@react-three/fiber";
import { Html, OrbitControls } from "@react-three/drei";
import * as THREE from "three";
import { getFloorView } from "../../api/spatial";
import { listAttachments, uploadAttachment, type Attachment } from "../../api/attachments";
import {
  createFloorPlanMarker,
  deleteFloorPlanMarker,
  listFloorPlanMarkers,
  type FloorPlanMarker,
  type FloorPlanMarkerEntityType,
} from "../../api/floorPlanMarkers";
import { listAssets, type Asset } from "../../api/assets";
import { listDevices, type Device } from "../../api/iot";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

const FLOOR_PLAN_PURPOSE = "FLOOR_PLAN_IMAGE";
// 跟 fms-attachment::handlers::MAX_UPLOAD_BYTES 對齊——那是伺服器端真正
// 擋下的上限（attachments 走 multipart 直接收進 API，不是像 BIM 那樣直傳），
// 這裡先在前端擋，不必等上傳完才因為 413 才知道選錯檔案。
const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

function isPdf(file: File): boolean {
  return file.type === "application/pdf" || file.name.toLowerCase().endsWith(".pdf");
}

function latestFloorPlanImage(attachments: Attachment[] | undefined): Attachment | undefined {
  return attachments
    ?.filter((a) => a.purpose === FLOOR_PLAN_PURPOSE)
    .sort((a, b) => (b.created_at ?? "").localeCompare(a.created_at ?? ""))[0];
}

async function pdfFirstPageToPngFile(file: File): Promise<File> {
  const pdfjsLib = await import("pdfjs-dist");
  const workerUrl = (await import("pdfjs-dist/build/pdf.worker.mjs?url")).default;
  pdfjsLib.GlobalWorkerOptions.workerSrc = workerUrl;

  const data = await file.arrayBuffer();
  const pdf = await pdfjsLib.getDocument({ data }).promise;
  const page = await pdf.getPage(1);
  const viewport = page.getViewport({ scale: 2 });
  const canvas = document.createElement("canvas");
  canvas.width = viewport.width;
  canvas.height = viewport.height;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("canvas 2d context unavailable");
  await page.render({ canvas, canvasContext: context, viewport }).promise;

  const blob: Blob = await new Promise((resolve, reject) =>
    canvas.toBlob((b) => (b ? resolve(b) : reject(new Error("toBlob failed"))), "image/png"),
  );
  return new File([blob], file.name.replace(/\.pdf$/i, ".png"), { type: "image/png" });
}

/// `useLoader` 丟出的錯誤（例如 `download_url` 是短效預簽網址，過期後
/// 404）會逃出 R3F 的 `Canvas`，沒有這層邊界會把整個 Facilities 頁面
/// 一起炸白，不是只有這個分頁看不到圖。
class TextureErrorBoundary extends Component<
  { fallback: ReactNode; children: ReactNode },
  { hasError: boolean }
> {
  state = { hasError: false };
  static getDerivedStateFromError() {
    return { hasError: true };
  }
  render() {
    return this.state.hasError ? this.props.fallback : this.props.children;
  }
}

export function FloorPlan3DTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const [floor, setFloor] = useState<number | null>(null);
  const [mode, setMode] = useState<"view" | "calibrate">("view");

  const floorViewQuery = useQuery({
    queryKey: ["floor-view", facilityId],
    queryFn: () => getFloorView(facilityId),
  });

  const floors = floorViewQuery.data?.meta?.floors ?? [];
  const activeFloor = floor ?? floors[0] ?? null;
  const floorNode = useMemo(
    () => (floorViewQuery.data?.data ?? []).find((n) => n.floor_level === activeFloor && n.node_type_code === "FLOOR"),
    [floorViewQuery.data, activeFloor],
  );

  const imagesQuery = useQuery({
    queryKey: ["floor-plan-image", floorNode?.id],
    queryFn: () => listAttachments("SPATIAL_NODE", floorNode!.id!),
    enabled: !!floorNode,
  });
  const markersQuery = useQuery({
    queryKey: ["floor-plan-markers", floorNode?.id],
    queryFn: () => listFloorPlanMarkers(floorNode!.id!),
    enabled: !!floorNode,
  });

  if (floorViewQuery.isLoading) {
    return (
      <div className="d-flex justify-content-center py-5">
        <div className="spinner-border text-primary" role="status" aria-label={t("facilities.loadingFloorView")} />
      </div>
    );
  }
  if (!floorNode) return <EmptyState title={t("facilities.noGeometryTitle")} />;

  const image = latestFloorPlanImage(imagesQuery.data?.data);
  const markers = markersQuery.data?.items ?? [];

  return (
    <div className="card">
      <div className="card-header">
        <div className="btn-group">
          {floors.map((f) => (
            <button key={f} type="button" className={`btn btn-sm ${f === activeFloor ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setFloor(f)}>
              {f === 0 ? "G" : `F${f}`}
            </button>
          ))}
        </div>
        <div className="btn-group ms-auto">
          <button type="button" className={`btn btn-sm ${mode === "view" ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setMode("view")}>
            {t("facilities.floorPlanViewMode")}
          </button>
          <Can permission="spatial_node:write">
            <button type="button" className={`btn btn-sm ${mode === "calibrate" ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setMode("calibrate")}>
              {t("facilities.floorPlanCalibrateMode")}
            </button>
          </Can>
        </div>
      </div>
      <div className="card-body">
        {mode === "calibrate" ? (
          <CalibrationPanel facilityId={facilityId} floorNodeId={floorNode.id!} image={image} markers={markers} />
        ) : (
          <Viewer3D image={image} markers={markers} />
        )}
      </div>
    </div>
  );
}

function CalibrationPanel({
  facilityId,
  floorNodeId,
  image,
  markers,
}: {
  facilityId: string;
  floorNodeId: string;
  image: Attachment | undefined;
  markers: FloorPlanMarker[];
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [entityType, setEntityType] = useState<FloorPlanMarkerEntityType>("ASSET");
  const [entityId, setEntityId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [uploading, setUploading] = useState(false);
  const imgRef = useRef<HTMLImageElement>(null);

  // 這兩個 key 刻意加 "-picker" 後綴，避免跟 AssetsListPage／DevicesTab
  // 既有的 useCursorList(["assets"/"devices", facilityId], ...) 撞到同一個
  // cache key——那兩個是 infinite query，形狀跟這裡的分頁查詢完全不同，
  // 撞到會讓其中一邊讀到另一邊的快取形狀，資料整個對不上。
  const assetsQuery = useQuery({ queryKey: ["assets-picker", facilityId], queryFn: () => listAssets({ facilityId, limit: 100 }), enabled: entityType === "ASSET" });
  const devicesQuery = useQuery({ queryKey: ["devices-picker", facilityId], queryFn: () => listDevices(facilityId), enabled: entityType === "DEVICE" });

  const invalidateImages = () => queryClient.invalidateQueries({ queryKey: ["floor-plan-image", floorNodeId] });
  const invalidateMarkers = () => queryClient.invalidateQueries({ queryKey: ["floor-plan-markers", floorNodeId] });

  const uploadMutation = useMutation({
    mutationFn: async (file: File) => {
      const uploadable = isPdf(file) ? await pdfFirstPageToPngFile(file) : file;
      return uploadAttachment("SPATIAL_NODE", floorNodeId, uploadable, FLOOR_PLAN_PURPOSE);
    },
    onMutate: () => setUploading(true),
    onSuccess: () => {
      setError(null);
      invalidateImages();
    },
    onError: (err) => setError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.floorPlanUploadError")),
    onSettled: () => setUploading(false),
  });

  const createMutation = useMutation({
    mutationFn: (body: { x_ratio: number; y_ratio: number }) =>
      createFloorPlanMarker(floorNodeId, { entity_type: entityType, entity_id: entityId, ...body }),
    onSuccess: () => {
      setError(null);
      invalidateMarkers();
    },
    onError: (err) => setError(err instanceof ApiError ? err.problem.detail ?? err.message : t("facilities.floorPlanMarkerError")),
  });
  const deleteMutation = useMutation({ mutationFn: (id: string) => deleteFloorPlanMarker(id), onSuccess: invalidateMarkers });

  function handleImageClick(e: React.MouseEvent<HTMLImageElement>) {
    if (!entityId || createMutation.isPending) {
      if (!entityId) setError(t("facilities.floorPlanSelectEntityFirst"));
      return;
    }
    const rect = e.currentTarget.getBoundingClientRect();
    const x_ratio = (e.clientX - rect.left) / rect.width;
    const y_ratio = (e.clientY - rect.top) / rect.height;
    createMutation.mutate({ x_ratio, y_ratio });
  }

  const entityOptions: { id: string; label: string }[] =
    entityType === "ASSET"
      ? (assetsQuery.data?.data ?? []).map((a: Asset) => ({ id: a.id!, label: a.name ?? a.asset_code ?? a.id! }))
      : (devicesQuery.data?.data ?? []).map((d: Device) => ({ id: d.id!, label: d.name ?? d.device_code ?? d.id! }));

  return (
    <div>
      {error && (
        <div className="alert alert-danger" onClick={() => setError(null)}>
          {error}
        </div>
      )}
      <div className="mb-3 d-flex flex-wrap gap-2 align-items-end">
        <div>
          <label className="form-label">{t("facilities.floorPlanUploadLabel")}</label>
          <input
            type="file"
            className="form-control"
            accept="image/png,image/jpeg,application/pdf"
            disabled={uploading}
            onChange={(e) => {
              const file = e.target.files?.[0];
              e.target.value = "";
              if (!file) return;
              const name = file.name.toLowerCase();
              if (!name.endsWith(".png") && !name.endsWith(".jpg") && !name.endsWith(".jpeg") && !isPdf(file)) {
                setError(t("facilities.floorPlanUploadInvalidFormat"));
                return;
              }
              if (file.size > MAX_UPLOAD_BYTES) {
                setError(t("facilities.floorPlanUploadTooLarge", { limit: Math.round(MAX_UPLOAD_BYTES / 1024 / 1024) }));
                return;
              }
              uploadMutation.mutate(file);
            }}
          />
        </div>
      </div>

      {!image ? (
        <EmptyState title={t("facilities.floorPlanNoImage")} />
      ) : (
        <>
          <div className="mb-3 d-flex flex-wrap gap-2 align-items-end">
            <div>
              <label className="form-label">{t("facilities.floorPlanEntityType")}</label>
              <select
                className="form-select"
                value={entityType}
                onChange={(e) => {
                  setEntityType(e.target.value as FloorPlanMarkerEntityType);
                  setEntityId("");
                }}
              >
                <option value="ASSET">{t("facilities.floorPlanEntityAsset")}</option>
                <option value="DEVICE">{t("facilities.floorPlanEntityDevice")}</option>
              </select>
            </div>
            <div>
              <label className="form-label">{t("facilities.floorPlanEntityPicker")}</label>
              <select className="form-select" value={entityId} onChange={(e) => setEntityId(e.target.value)}>
                <option value="">{t("facilities.floorPlanSelectEntity")}</option>
                {entityOptions.map((o) => (
                  <option value={o.id} key={o.id}>
                    {o.label}
                  </option>
                ))}
              </select>
            </div>
            <div className="text-secondary small">{t("facilities.floorPlanClickHint")}</div>
          </div>

          <div style={{ position: "relative", display: "inline-block", maxWidth: "100%" }}>
            <img
              ref={imgRef}
              src={image.download_url!}
              alt={t("facilities.floorPlanImageAlt")}
              style={{ maxWidth: "100%", display: "block", cursor: entityId ? "crosshair" : "default" }}
              onClick={handleImageClick}
            />
            {markers.map((m) => (
              <button
                key={m.id}
                type="button"
                title={`${m.entity_label ?? m.entity_id} (${t("common.delete")})`}
                onClick={(e) => {
                  e.stopPropagation();
                  if (!window.confirm(t("facilities.confirmDeleteFloorPlanMarker", { name: m.entity_label ?? m.entity_id }))) return;
                  deleteMutation.mutate(m.id);
                }}
                style={{
                  position: "absolute",
                  left: `${m.x_ratio * 100}%`,
                  top: `${m.y_ratio * 100}%`,
                  transform: "translate(-50%, -50%)",
                  width: 14,
                  height: 14,
                  borderRadius: "50%",
                  border: "2px solid #fff",
                  background: "#4bbb95",
                  padding: 0,
                  cursor: "pointer",
                }}
              />
            ))}
          </div>
        </>
      )}
    </div>
  );
}

function FloorSandbox({ imageUrl, markers }: { imageUrl: string; markers: FloorPlanMarker[] }) {
  const texture = useLoader(THREE.TextureLoader, imageUrl);
  const width = 16;
  const depth = 12;
  return (
    <>
      <mesh rotation={[-Math.PI / 2, 0, 0]} receiveShadow>
        <boxGeometry args={[width, depth, 0.2]} />
        <meshStandardMaterial map={texture} roughness={0.5} />
      </mesh>
      {markers.map((m) => (
        <EquipmentMarker key={m.id} marker={m} width={width} depth={depth} />
      ))}
    </>
  );
}

function EquipmentMarker({ marker, width, depth }: { marker: FloorPlanMarker; width: number; depth: number }) {
  const [hovered, setHovered] = useState(false);
  const position: [number, number, number] = [
    (marker.x_ratio - 0.5) * width,
    0.2 + marker.z_offset,
    (marker.y_ratio - 0.5) * depth,
  ];
  const isDown = marker.entity_status === "DOWN" || marker.entity_status === "OFFLINE" || marker.entity_status === "FAULT";
  const color = isDown ? "#e0703f" : "#4bbb95";
  return (
    <mesh position={position} onPointerOver={() => setHovered(true)} onPointerOut={() => setHovered(false)}>
      <sphereGeometry args={[0.18, 16, 16]} />
      <meshStandardMaterial color={color} emissive={color} emissiveIntensity={hovered ? 1.5 : 0.6} />
      {hovered && (
        <Html distanceFactor={10} position={[0, 0.4, 0]} center>
          <div style={{ background: "rgba(0,0,0,0.85)", color: "#fff", padding: "4px 8px", borderRadius: 4, whiteSpace: "nowrap", fontSize: 12 }}>
            <strong>{marker.entity_label ?? marker.entity_id}</strong>
            {marker.entity_status && <div>{marker.entity_status}</div>}
          </div>
        </Html>
      )}
    </mesh>
  );
}

function Viewer3D({ image, markers }: { image: Attachment | undefined; markers: FloorPlanMarker[] }) {
  const { t } = useTranslation();
  if (!image) return <EmptyState title={t("facilities.floorPlanNoImage")} />;
  return (
    <div style={{ width: "100%", height: 520, background: "#1a1a1a", borderRadius: 4 }}>
      <TextureErrorBoundary
        key={image.download_url}
        fallback={
          <div className="d-flex align-items-center justify-content-center h-100 text-white-50">
            {t("facilities.floorPlanImageLoadError")}
          </div>
        }
      >
        <Canvas camera={{ position: [0, 14, 14], fov: 50 }}>
          <ambientLight intensity={0.6} />
          <directionalLight position={[10, 20, 10]} intensity={1.2} />
          <Suspense fallback={null}>
            <FloorSandbox imageUrl={image.download_url!} markers={markers} />
          </Suspense>
          <OrbitControls maxPolarAngle={Math.PI / 2.1} minDistance={5} maxDistance={30} />
        </Canvas>
      </TextureErrorBoundary>
    </div>
  );
}
