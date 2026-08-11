import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { createFacility, deleteFacility, listFacilities, updateFacility, type Facility } from "../../api/facilities";
import { listOrganizations } from "../../api/organizations";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

const FACILITY_TYPES = ["OFFICE", "CINEMA", "CAMPUS", "FACTORY", "WAREHOUSE", "HOSPITAL", "MALL", "DATACENTER", "MIXED", "OTHER"];

export function FacilitiesTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  const query = useQuery({ queryKey: ["facilities"], queryFn: listFacilities });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["facilities"] });
  }

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteFacility(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("admin.deleteFacilityError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("admin.facilitiesTitle")}</h3>
        <Can permission="facility:create">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("admin.newFacility")}
          </button>
        </Can>
      </div>
      {showForm && (
        <FacilityForm
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
              <th>{t("admin.code")}</th>
              <th>{t("common.name")}</th>
              <th>{t("admin.facilityType")}</th>
              <th>{t("admin.colCity")}</th>
              <th>{t("common.status")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {query.data?.data?.map((facility) => (
              <Fragment key={facility.id}>
                <tr>
                  <td>
                    <code>{facility.code}</code>
                  </td>
                  <td>{facility.name}</td>
                  <td className="text-secondary">{facility.facility_type}</td>
                  <td className="text-secondary">{facility.city ?? "—"}</td>
                  <td>
                    <span className="badge bg-blue-lt">{facility.status}</span>
                  </td>
                  <td className="text-end">
                    <Can permission="facility:update">
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === facility.id ? null : (facility.id ?? null))}>
                        {t("common.edit")}
                      </button>
                      <button
                        type="button"
                        className="btn btn-sm btn-outline-danger"
                        disabled={deleteMutation.isPending}
                        title={t("admin.deleteFacilityBlockedHint")}
                        onClick={() => {
                          if (window.confirm(t("admin.confirmDeleteFacility", { name: facility.name }))) deleteMutation.mutate(facility.id!);
                        }}
                      >
                        {t("common.delete")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {editingId === facility.id && (
                  <tr>
                    <td colSpan={6} className="bg-body-tertiary">
                      <FacilityForm
                        facility={facility}
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
      {!query.isLoading && query.data?.data?.length === 0 && <EmptyState title={t("admin.noFacilities")} />}
    </div>
  );
}

function FacilityForm({ facility, onDone }: { facility?: Facility; onDone: () => void }) {
  const { t } = useTranslation();
  const isEdit = !!facility;
  const orgsQuery = useQuery({ queryKey: ["organizations"], queryFn: listOrganizations, enabled: !isEdit });
  const [orgId, setOrgId] = useState("");
  const [code, setCode] = useState(facility?.code ?? "");
  const [name, setName] = useState(facility?.name ?? "");
  const [facilityType, setFacilityType] = useState(facility?.facility_type ?? "OFFICE");
  const [city, setCity] = useState(facility?.city ?? "");

  const mutation = useMutation({
    mutationFn: () =>
      isEdit
        ? updateFacility(facility!.id!, { name, facility_type: facilityType, city: city || undefined })
        : createFacility({ org_id: orgId, code, name, facility_type: facilityType, city: city || undefined, timezone: "Asia/Taipei" }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? (mutation.error.problem.detail ?? mutation.error.message) : t("admin.saveFacilityError")}</div>
      )}
      <div className="row g-2">
        {!isEdit && (
          <div className="col-md-3">
            <label className="form-label">{t("admin.organization")}</label>
            <select className="form-select" value={orgId} onChange={(e) => setOrgId(e.target.value)}>
              <option value="">{t("facilities.selectPlaceholder")}</option>
              {orgsQuery.data?.data?.map((o) => (
                <option value={o.id} key={o.id}>
                  {o.name}
                </option>
              ))}
            </select>
          </div>
        )}
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("admin.code")}</label>
            <input className="form-control" value={code} onChange={(e) => setCode(e.target.value.toUpperCase())} />
          </div>
        )}
        <div className="col-md-2">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("admin.facilityType")}</label>
          <select className="form-select" value={facilityType} onChange={(e) => setFacilityType(e.target.value)}>
            {FACILITY_TYPES.map((ft) => (
              <option value={ft} key={ft}>
                {ft}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("admin.colCity")}</label>
          <input className="form-control" value={city} onChange={(e) => setCity(e.target.value)} />
        </div>
        <div className="col-md-1 d-flex align-items-end">
          <button
            type="button"
            className="btn btn-primary w-100"
            disabled={mutation.isPending || !name || (!isEdit && (!orgId || !code))}
            onClick={() => mutation.mutate()}
          >
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
