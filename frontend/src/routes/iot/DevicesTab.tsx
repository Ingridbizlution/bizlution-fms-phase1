import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { listSpatialNodes } from "../../api/spatial";
import { ApiError } from "../../api/client";
import { createDevice, decommissionDevice, listDevices, updateDevice, type Device } from "../../api/iot";
import { Can } from "../../auth/Can";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";

const DEVICE_TYPES = ["SENSOR", "METER", "CONTROLLER", "ACCESS_PANEL", "CAMERA", "OCCUPANCY", "ENVIRONMENT", "GATEWAY"];

function connectivityBadge(state: string | undefined): string {
  switch (state) {
    case "ONLINE":
      return "bg-green-lt";
    case "OFFLINE":
      return "bg-red-lt";
    case "MAINTENANCE":
      return "bg-yellow-lt";
    case "DISABLED":
      return "bg-secondary-lt";
    default:
      return "bg-secondary-lt";
  }
}

export function DevicesTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);
  const { items, isLoading, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(["devices", facilityId], (cursor) => listDevices(facilityId, cursor));

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["devices", facilityId] });
  }

  const decommissionMutation = useMutation({
    mutationFn: (id: string) => decommissionDevice(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("iot.decommissionError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("iot.tabDevices")}</h3>
        <Can permission="device:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("iot.registerDevice")}
          </button>
        </Can>
      </div>
      {showForm && (
        <DeviceForm
          facilityId={facilityId}
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
              <th>{t("iot.colDevice")}</th>
              <th>{t("iot.colType")}</th>
              <th>{t("iot.colLocation")}</th>
              <th>{t("iot.colConnectivity")}</th>
              <th>{t("iot.colLastSeen")}</th>
              <th>{t("iot.colPoints")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((d) => (
              <Fragment key={d.id}>
                <tr>
                  <td>
                    <code>{d.device_code}</code>
                    <div className="text-secondary small">{d.name}</div>
                  </td>
                  <td className="text-secondary">{d.device_type}</td>
                  <td>{d.location_name ?? d.asset_code ?? "—"}</td>
                  <td>
                    <span className={`badge ${connectivityBadge(d.connectivity)}`}>{d.connectivity}</span>
                  </td>
                  <td className="text-secondary">{d.seconds_since_seen != null ? t("iot.minutesAgo", { count: Math.round(d.seconds_since_seen / 60) }) : t("iot.never")}</td>
                  <td>{d.point_count ?? 0}</td>
                  <td className="text-end">
                    <Can permission="device:write">
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === d.id ? null : d.id!)}>
                        {t("common.edit")}
                      </button>
                      <button
                        type="button"
                        className="btn btn-sm btn-outline-danger"
                        disabled={decommissionMutation.isPending}
                        title={t("iot.decommissionBlockedHint")}
                        onClick={() => {
                          if (window.confirm(t("iot.confirmDecommission", { name: d.name }))) decommissionMutation.mutate(d.id!);
                        }}
                      >
                        {t("iot.decommission")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {editingId === d.id && (
                  <tr>
                    <td colSpan={7} className="bg-body-tertiary">
                      <DeviceForm
                        device={d}
                        facilityId={facilityId}
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
      {!isLoading && items.length === 0 && <EmptyState title={t("iot.noDevices")} />}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}

function DeviceForm({ device, facilityId, onDone }: { device?: Device; facilityId: string; onDone: () => void }) {
  const { t } = useTranslation();
  const isEdit = !!device;
  const nodesQuery = useQuery({ queryKey: ["spatial-nodes-picker", facilityId], queryFn: () => listSpatialNodes(facilityId), enabled: !isEdit });
  const [deviceCode, setDeviceCode] = useState(device?.device_code ?? "");
  const [name, setName] = useState(device?.name ?? "");
  const [deviceType, setDeviceType] = useState<string>(device?.device_type ?? "SENSOR");
  const [spatialNodeId, setSpatialNodeId] = useState("");
  const [address, setAddress] = useState(device?.address ?? "");
  const [offlineAfterSeconds, setOfflineAfterSeconds] = useState(device?.offline_alarm_after_seconds ?? 900);

  const mutation = useMutation({
    mutationFn: () =>
      isEdit
        ? updateDevice(device!.id!, { name, device_type: deviceType, address, offline_alarm_after_seconds: offlineAfterSeconds })
        : createDevice({ facility_id: facilityId, device_code: deviceCode, name, device_type: deviceType, spatial_node_id: spatialNodeId, address: address || undefined }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("iot.saveDeviceError")}</div>
      )}
      <div className="row g-2">
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("iot.deviceCode")}</label>
            <input className="form-control" value={deviceCode} onChange={(e) => setDeviceCode(e.target.value)} />
          </div>
        )}
        <div className="col-md-2">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("iot.colType")}</label>
          <select className="form-select" value={deviceType} onChange={(e) => setDeviceType(e.target.value)}>
            {DEVICE_TYPES.map((dt) => (
              <option value={dt} key={dt}>
                {dt}
              </option>
            ))}
          </select>
        </div>
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("iot.colLocation")}</label>
            <select className="form-select" value={spatialNodeId} onChange={(e) => setSpatialNodeId(e.target.value)}>
              <option value="">{t("facilities.selectPlaceholder")}</option>
              {nodesQuery.data?.data?.map((n) => (
                <option value={n.id} key={n.id}>
                  {n.name}
                </option>
              ))}
            </select>
          </div>
        )}
        <div className="col-md-2">
          <label className="form-label">{t("iot.address")}</label>
          <input className="form-control" value={address} onChange={(e) => setAddress(e.target.value)} />
        </div>
        {isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("iot.offlineAfterSeconds")}</label>
            <input type="number" min={1} max={86400} className="form-control" value={offlineAfterSeconds} onChange={(e) => setOfflineAfterSeconds(Number(e.target.value))} />
          </div>
        )}
      </div>
      <div className="mt-2 d-flex gap-2">
        <button
          type="button"
          className="btn btn-primary"
          disabled={mutation.isPending || !name || (!isEdit && (!deviceCode || !spatialNodeId))}
          onClick={() => mutation.mutate()}
        >
          {t("common.save")}
        </button>
        <button type="button" className="btn btn-outline-secondary" onClick={onDone}>
          {t("common.cancel")}
        </button>
      </div>
    </div>
  );
}
