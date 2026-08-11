import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import {
  createAssetModel,
  deleteAssetModel,
  listAssetCategories,
  listAssetModels,
  updateAssetModel,
  type AssetModel,
} from "../../api/assets";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

function protocolsFromText(text: string): string[] {
  return text
    .split(",")
    .map((s) => s.trim().toUpperCase())
    .filter(Boolean);
}

export function AssetModelsPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [showInactive, setShowInactive] = useState(false);
  const [rowError, setRowError] = useState<string | null>(null);

  const modelsQuery = useQuery({
    queryKey: ["asset-models", showInactive],
    queryFn: () => listAssetModels({ isActive: showInactive ? false : undefined }),
  });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["asset-models"] });
  }

  const toggleActiveMutation = useMutation({
    mutationFn: (model: AssetModel) => updateAssetModel(model.id!, { is_active: !model.is_active }),
    onSuccess: invalidate,
  });

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteAssetModel(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("assetModels.deleteError")),
  });

  return (
    <>
      <PageHeader
        title={t("assetModels.pageTitle")}
        actions={
          <Link to="/assets" className="btn btn-outline-secondary">
            {t("assetModels.backToAssets")}
          </Link>
        }
      />
      <PageBody>
        <div className="card">
          <div className="card-header">
            <div className="form-check form-switch">
              <input className="form-check-input" type="checkbox" checked={showInactive} onChange={(e) => setShowInactive(e.target.checked)} id="show-inactive-models" />
              <label className="form-check-label" htmlFor="show-inactive-models">
                {t("assetModels.showInactive")}
              </label>
            </div>
            <Can permission="asset_model:write">
              <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
                {showForm ? t("common.cancel") : t("assetModels.newModel")}
              </button>
            </Can>
          </div>
          {showForm && (
            <ModelForm
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
                  <th>{t("assetModels.colManufacturerModel")}</th>
                  <th>{t("common.name")}</th>
                  <th>{t("assetModels.colCategory")}</th>
                  <th>{t("assetModels.colProtocols")}</th>
                  <th>{t("assetModels.colExpectedLife")}</th>
                  <th>{t("common.status")}</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {modelsQuery.data?.data?.map((model) => (
                  <Fragment key={model.id}>
                    <tr>
                      <td>
                        <code>
                          {model.manufacturer} / {model.model_no}
                        </code>
                      </td>
                      <td>{model.name}</td>
                      <td className="text-secondary">{model.category_code}</td>
                      <td className="text-secondary">{model.supported_protocols?.join("、") || "—"}</td>
                      <td>{model.expected_life_months ?? "—"}</td>
                      <td>
                        <span className={`badge ${model.is_active ? "bg-green-lt" : "bg-secondary-lt"}`}>
                          {model.is_active ? t("maintenance.active") : t("maintenance.inactive")}
                        </span>
                      </td>
                      <td className="text-end">
                        <Can permission="asset_model:write">
                          <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === model.id ? null : model.id!)}>
                            {t("common.edit")}
                          </button>
                          <button
                            type="button"
                            className={`btn btn-sm me-1 ${model.is_active ? "btn-outline-danger" : "btn-outline-success"}`}
                            disabled={toggleActiveMutation.isPending}
                            onClick={() => toggleActiveMutation.mutate(model)}
                          >
                            {model.is_active ? t("maintenance.deactivate") : t("maintenance.activate")}
                          </button>
                          <button
                            type="button"
                            className="btn btn-sm btn-outline-danger"
                            disabled={deleteMutation.isPending}
                            title={t("assetModels.deleteBlockedHint")}
                            onClick={() => {
                              if (window.confirm(t("assetModels.confirmDelete", { name: model.name }))) deleteMutation.mutate(model.id!);
                            }}
                          >
                            {t("common.delete")}
                          </button>
                        </Can>
                      </td>
                    </tr>
                    {editingId === model.id && (
                      <tr>
                        <td colSpan={7} className="bg-body-tertiary">
                          <ModelForm
                            model={model}
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
          {!modelsQuery.isLoading && modelsQuery.data?.data?.length === 0 && <EmptyState title={t("assetModels.noModels")} />}
        </div>
      </PageBody>
    </>
  );
}

function ModelForm({ model, onDone }: { model?: AssetModel; onDone: () => void }) {
  const { t } = useTranslation();
  const isEdit = !!model;
  const categoriesQuery = useQuery({ queryKey: ["asset-categories"], queryFn: listAssetCategories, enabled: !isEdit });
  const [categoryId, setCategoryId] = useState("");
  const [manufacturer, setManufacturer] = useState(model?.manufacturer ?? "");
  const [modelNo, setModelNo] = useState(model?.model_no ?? "");
  const [name, setName] = useState(model?.name ?? "");
  const [protocolsText, setProtocolsText] = useState(model?.supported_protocols?.join(", ") ?? "");
  const [expectedLifeMonths, setExpectedLifeMonths] = useState(model?.expected_life_months ?? undefined);

  const mutation = useMutation({
    mutationFn: () => {
      const supported_protocols = protocolsFromText(protocolsText);
      return isEdit
        ? updateAssetModel(model!.id!, { name, supported_protocols, expected_life_months: expectedLifeMonths })
        : createAssetModel({ category_id: categoryId, manufacturer, model_no: modelNo, name, supported_protocols, expected_life_months: expectedLifeMonths });
    },
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("assetModels.saveError")}</div>
      )}
      <div className="row g-2">
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("assetModels.colCategory")}</label>
            <select className="form-select" value={categoryId} onChange={(e) => setCategoryId(e.target.value)}>
              <option value="">{t("facilities.selectPlaceholder")}</option>
              {categoriesQuery.data?.data?.map((c) => (
                <option value={c.id} key={c.id}>
                  {c.name}
                </option>
              ))}
            </select>
          </div>
        )}
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("assetModels.manufacturer")}</label>
            <input className="form-control" value={manufacturer} onChange={(e) => setManufacturer(e.target.value)} />
          </div>
        )}
        {!isEdit && (
          <div className="col-md-2">
            <label className="form-label">{t("assetModels.modelNo")}</label>
            <input className="form-control" value={modelNo} onChange={(e) => setModelNo(e.target.value)} />
          </div>
        )}
        <div className="col-md-2">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("assetModels.colProtocols")}</label>
          <input className="form-control" placeholder="MQTT, MODBUS_TCP" value={protocolsText} onChange={(e) => setProtocolsText(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("assetModels.colExpectedLife")}</label>
          <input
            type="number"
            min={1}
            className="form-control"
            value={expectedLifeMonths ?? ""}
            onChange={(e) => setExpectedLifeMonths(e.target.value ? Number(e.target.value) : undefined)}
          />
        </div>
      </div>
      <div className="mt-2 d-flex gap-2">
        <button
          type="button"
          className="btn btn-primary"
          disabled={mutation.isPending || !name || (!isEdit && (!categoryId || !manufacturer || !modelNo))}
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
