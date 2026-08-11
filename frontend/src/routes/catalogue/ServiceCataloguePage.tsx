import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { ApiError } from "../../api/client";
import { createServiceItem, deactivateServiceItem, listServiceItems, updateServiceItem, type ServiceItem } from "../../api/serviceItems";
import { useAuth } from "../../auth/AuthContext";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

const CATEGORIES = [
  { value: "CLEANING", labelKey: "catalogue.categoryCleaning" },
  { value: "CATERING", labelKey: "catalogue.categoryCatering" },
  { value: "IT_SUPPORT", labelKey: "catalogue.categoryItSupport" },
  { value: "ROOM_SETUP", labelKey: "catalogue.categoryRoomSetup" },
  { value: "SECURITY", labelKey: "catalogue.categorySecurity" },
  { value: "MOVING", labelKey: "catalogue.categoryMoving" },
  { value: "WASTE", labelKey: "catalogue.categoryWaste" },
  { value: "LANDSCAPING", labelKey: "catalogue.categoryLandscaping" },
  { value: "AV_SUPPORT", labelKey: "catalogue.categoryAvSupport" },
  { value: "RECEPTION", labelKey: "catalogue.categoryReception" },
  { value: "OTHER", labelKey: "catalogue.categoryOther" },
];

export function ServiceCataloguePage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [showForm, setShowForm] = useState(false);

  return (
    <>
      <PageHeader
        title={t("catalogue.pageTitle")}
        actions={
          <Can permission="service_item:write">
            <button type="button" className="btn btn-primary" onClick={() => setShowForm((s) => !s)}>
              {showForm ? t("common.cancel") : t("catalogue.newService")}
            </button>
          </Can>
        }
      />
      <PageBody>
        {!facilityId ? (
          <EmptyState title={t("catalogue.noFacilitySelected")} />
        ) : (
          <div className="card">
            {showForm && <ServiceItemForm facilityId={facilityId} onDone={() => setShowForm(false)} />}
            <ServiceItemsTable facilityId={facilityId} />
          </div>
        )}
      </PageBody>
    </>
  );
}

function ServiceItemsTable({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data, isLoading } = useQuery({ queryKey: ["service-items", facilityId], queryFn: () => listServiceItems(facilityId) });
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["service-items", facilityId] });
  }

  const deactivateMutation = useMutation({
    mutationFn: (id: string) => deactivateServiceItem(id),
    onSuccess: (res) => {
      void invalidate();
      const openWorkOrders = res.data?.open_work_orders ?? 0;
      if (openWorkOrders > 0) window.alert(t("catalogue.deactivatedWithOpenWorkOrders", { count: openWorkOrders }));
    },
    onError: (err) => setRowError(err instanceof ApiError ? (err.problem.detail ?? err.message) : t("catalogue.deactivateError")),
  });

  return (
    <div className="table-responsive">
      {rowError && (
        <div className="alert alert-danger m-3 mb-0" onClick={() => setRowError(null)}>
          {rowError}
        </div>
      )}
      <table className="table table-vcenter card-table">
        <thead>
          <tr>
            <th>{t("catalogue.colCode")}</th>
            <th>{t("catalogue.colName")}</th>
            <th>{t("catalogue.colCategory")}</th>
            <th>{t("catalogue.colDuration")}</th>
            <th>{t("catalogue.colRequiresApproval")}</th>
            <th>{t("catalogue.colChargeable")}</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {data?.data?.map((item) => (
            <Fragment key={item.id}>
              <tr>
                <td>
                  <code>{item.code}</code>
                </td>
                <td>
                  {item.name}
                  <div className="text-secondary small">{item.description}</div>
                </td>
                <td className="text-secondary">{item.category}</td>
                <td>{item.default_duration_minutes} min</td>
                <td>{item.requires_approval ? <span className="badge bg-yellow-lt">{t("common.yes")}</span> : "—"}</td>
                <td>{item.chargeable ? `${item.unit_price ?? 0} ${item.currency ?? ""}` : "—"}</td>
                <td className="text-end">
                  <Can permission="service_item:write">
                    <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === item.id ? null : (item.id ?? null))}>
                      {t("common.edit")}
                    </button>
                    <button
                      type="button"
                      className="btn btn-sm btn-outline-danger"
                      disabled={deactivateMutation.isPending}
                      title={t("catalogue.deactivateBlockedHint")}
                      onClick={() => {
                        if (window.confirm(t("catalogue.confirmDeactivate", { name: item.name }))) deactivateMutation.mutate(item.id!);
                      }}
                    >
                      {t("catalogue.deactivate")}
                    </button>
                  </Can>
                </td>
              </tr>
              {editingId === item.id && (
                <tr>
                  <td colSpan={7} className="bg-body-tertiary">
                    <ServiceItemForm
                      item={item}
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
      {!isLoading && data?.data?.length === 0 && <EmptyState title={t("catalogue.noServiceItems")} />}
    </div>
  );
}

function ServiceItemForm({ facilityId, item, onDone }: { facilityId?: string; item?: ServiceItem; onDone: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const isEdit = !!item;
  const [code, setCode] = useState("");
  const [name, setName] = useState(item?.name ?? "");
  const [category, setCategory] = useState(item?.category ?? "OTHER");
  const [requiresApproval, setRequiresApproval] = useState(item?.requires_approval ?? false);

  const mutation = useMutation({
    mutationFn: () =>
      isEdit
        ? updateServiceItem(item!.id!, { name, category, requires_approval: requiresApproval })
        : createServiceItem(facilityId!, { code, name, category, requires_approval: requiresApproval }),
    onSuccess: () => {
      if (!isEdit) queryClient.invalidateQueries({ queryKey: ["service-items", facilityId] });
      onDone();
    },
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">
          {mutation.error instanceof ApiError ? (mutation.error.problem.detail ?? mutation.error.message) : t(isEdit ? "catalogue.saveError" : "catalogue.createError")}
        </div>
      )}
      <div className="row g-2 align-items-end">
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("catalogue.code")}</label>
            <input className="form-control" value={code} onChange={(e) => setCode(e.target.value)} />
          </div>
        )}
        <div className="col-md-3">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("catalogue.category")}</label>
          <select className="form-select" value={category} onChange={(e) => setCategory(e.target.value)}>
            {CATEGORIES.map((c) => (
              <option value={c.value} key={c.value}>
                {t(c.labelKey)}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-3">
          <label className="form-check">
            <input type="checkbox" className="form-check-input" checked={requiresApproval} onChange={(e) => setRequiresApproval(e.target.checked)} />
            <span className="form-check-label">{t("catalogue.requiresApproval")}</span>
          </label>
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending || !name || (!isEdit && !code)} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
        {isEdit && (
          <div className="col-md-auto">
            <button type="button" className="btn btn-outline-secondary" onClick={onDone}>
              {t("common.cancel")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
