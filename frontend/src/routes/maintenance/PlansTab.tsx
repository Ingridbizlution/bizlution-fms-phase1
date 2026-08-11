import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { listAssets } from "../../api/assets";
import { ApiError } from "../../api/client";
import {
  createMaintenancePlan,
  generateMaintenanceNow,
  listMaintenancePlans,
  listMaintenanceTemplates,
  previewMaintenanceSchedule,
  updateMaintenancePlan,
  type MaintenancePlan,
} from "../../api/maintenance";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

export function PlansTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [previewingPlanId, setPreviewingPlanId] = useState<string | null>(null);
  const [editingPlanId, setEditingPlanId] = useState<string | null>(null);
  const [genMessage, setGenMessage] = useState<string | null>(null);

  const plansQuery = useQuery({ queryKey: ["maintenance-plans", facilityId], queryFn: () => listMaintenancePlans(facilityId) });

  const toggleActiveMutation = useMutation({
    mutationFn: (plan: MaintenancePlan) => updateMaintenancePlan(plan.id!, { is_active: !plan.is_active }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["maintenance-plans", facilityId] }),
  });

  const previewQuery = useQuery({
    queryKey: ["maintenance-preview", previewingPlanId],
    queryFn: () => previewMaintenanceSchedule(previewingPlanId!),
    enabled: !!previewingPlanId,
  });

  const generateMutation = useMutation({
    mutationFn: (planId: string) => generateMaintenanceNow(planId),
    onSuccess: (res) => {
      setGenMessage(res.created ? t("maintenance.createdWorkOrders", { count: res.created }) : t("maintenance.nothingToGenerate", { skipped: res.skipped ?? 0 }));
      queryClient.invalidateQueries({ queryKey: ["maintenance-occurrences"] });
    },
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("maintenance.pmPlans")}</h3>
        <Can permission="maintenance_plan:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("maintenance.newPlan")}
          </button>
        </Can>
      </div>
      {showForm && (
        <NewPlanForm
          facilityId={facilityId}
          onDone={() => {
            setShowForm(false);
            queryClient.invalidateQueries({ queryKey: ["maintenance-plans", facilityId] });
          }}
        />
      )}
      {genMessage && (
        <div className="alert alert-success m-3 mb-0" onClick={() => setGenMessage(null)}>
          {genMessage}
        </div>
      )}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("maintenance.colPlan")}</th>
              <th>{t("maintenance.colTemplate")}</th>
              <th>{t("maintenance.colTrigger")}</th>
              <th>{t("maintenance.colNextDue")}</th>
              <th>{t("common.status")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {plansQuery.data?.data?.map((plan) => (
              <Fragment key={plan.id}>
                <tr>
                  <td>
                    <code>{plan.code}</code>
                    <div>{plan.name}</div>
                    <div className="text-secondary small">{plan.target?.label}</div>
                  </td>
                  <td className="text-secondary">{plan.template_name}</td>
                  <td>{plan.trigger_type}</td>
                  <td>{plan.next_due_at ? new Date(plan.next_due_at).toLocaleDateString() : "—"}</td>
                  <td>
                    <span className={`badge ${plan.is_active ? "bg-green-lt" : "bg-secondary-lt"}`}>
                      {plan.is_active ? t("maintenance.active") : t("maintenance.inactive")}
                    </span>
                  </td>
                  <td className="text-end">
                    <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setPreviewingPlanId(previewingPlanId === plan.id ? null : plan.id!)}>
                      {t("maintenance.preview")}
                    </button>
                    <Can permission="maintenance_plan:write">
                      <button type="button" className="btn btn-sm btn-outline-primary me-1" disabled={generateMutation.isPending} onClick={() => generateMutation.mutate(plan.id!)}>
                        {t("maintenance.generateNow")}
                      </button>
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingPlanId(editingPlanId === plan.id ? null : plan.id!)}>
                        {t("common.edit")}
                      </button>
                      <button
                        type="button"
                        className={`btn btn-sm ${plan.is_active ? "btn-outline-danger" : "btn-outline-success"}`}
                        disabled={toggleActiveMutation.isPending}
                        onClick={() => toggleActiveMutation.mutate(plan)}
                      >
                        {plan.is_active ? t("maintenance.deactivate") : t("maintenance.activate")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {previewingPlanId === plan.id && (
                  <tr>
                    <td colSpan={6} className="bg-body-tertiary">
                      {previewQuery.isLoading ? (
                        t("maintenance.loadingPreview")
                      ) : previewQuery.data?.data?.length ? (
                        <div className="d-flex flex-wrap gap-1">
                          {previewQuery.data.data.map((occ, i) => (
                            <span className="badge bg-secondary-lt" key={i}>
                              {new Date(occ.scheduled_for!).toLocaleDateString()}
                            </span>
                          ))}
                        </div>
                      ) : (
                        <span className="text-secondary">{t("maintenance.noUpcomingOccurrences")}</span>
                      )}
                    </td>
                  </tr>
                )}
                {editingPlanId === plan.id && (
                  <tr>
                    <td colSpan={6} className="bg-body-tertiary">
                      <EditPlanForm
                        plan={plan}
                        onDone={() => {
                          setEditingPlanId(null);
                          queryClient.invalidateQueries({ queryKey: ["maintenance-plans", facilityId] });
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
      {!plansQuery.isLoading && plansQuery.data?.data?.length === 0 && <EmptyState title={t("maintenance.noPmPlans")} />}
    </div>
  );
}

function EditPlanForm({ plan, onDone }: { plan: MaintenancePlan; onDone: () => void }) {
  const { t } = useTranslation();
  const [name, setName] = useState(plan.name ?? "");
  const [rrule, setRrule] = useState(plan.rrule ?? "");
  const [priority, setPriority] = useState(plan.priority ?? "MEDIUM");
  const [generateLeadDays, setGenerateLeadDays] = useState(plan.generate_lead_days ?? 7);

  const mutation = useMutation({
    mutationFn: () =>
      updateMaintenancePlan(plan.id!, {
        name,
        rrule: plan.trigger_type === "CALENDAR" ? rrule : undefined,
        priority,
        generate_lead_days: generateLeadDays,
      }),
    onSuccess: onDone,
  });

  return (
    <div>
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("maintenance.updatePlanError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-3">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        {plan.trigger_type === "CALENDAR" && (
          <div className="col-md-3">
            <label className="form-label">{t("maintenance.recurrenceRrule")}</label>
            <input className="form-control" value={rrule} onChange={(e) => setRrule(e.target.value)} />
          </div>
        )}
        <div className="col-md-2">
          <label className="form-label">{t("maintenance.priority")}</label>
          <select className="form-select" value={priority} onChange={(e) => setPriority(e.target.value)}>
            {["LOW", "MEDIUM", "HIGH", "URGENT", "CRITICAL"].map((p) => (
              <option value={p} key={p}>
                {p}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("maintenance.generateLeadDays")}</label>
          <input type="number" min={0} max={365} className="form-control" value={generateLeadDays} onChange={(e) => setGenerateLeadDays(Number(e.target.value))} />
        </div>
        <div className="col-md-2 d-flex gap-1">
          <button type="button" className="btn btn-primary flex-fill" disabled={mutation.isPending || !name} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
          <button type="button" className="btn btn-outline-secondary flex-fill" onClick={onDone}>
            {t("common.cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}

function NewPlanForm({ facilityId, onDone }: { facilityId: string; onDone: () => void }) {
  const { t } = useTranslation();
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [templateId, setTemplateId] = useState("");
  const [assetId, setAssetId] = useState("");
  const [rrule, setRrule] = useState("FREQ=MONTHLY;INTERVAL=1");

  const templatesQuery = useQuery({ queryKey: ["maintenance-templates"], queryFn: listMaintenanceTemplates });
  const assetsQuery = useQuery({ queryKey: ["assets-picker", facilityId], queryFn: () => listAssets({ facilityId, limit: 50 }) });

  const mutation = useMutation({
    mutationFn: () =>
      createMaintenancePlan({
        facility_id: facilityId,
        template_id: templateId,
        code,
        name,
        asset_id: assetId || undefined,
        trigger_type: "CALENDAR",
        rrule,
        generate_lead_days: 7,
        priority: "MEDIUM",
      }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("maintenance.createPlanError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-2">
          <label className="form-label">{t("maintenance.code")}</label>
          <input className="form-control" value={code} onChange={(e) => setCode(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("maintenance.template")}</label>
          <select className="form-select" value={templateId} onChange={(e) => setTemplateId(e.target.value)}>
            <option value="">{t("facilities.selectPlaceholder")}</option>
            {templatesQuery.data?.data?.map((tpl) => (
              <option value={tpl.id} key={tpl.id}>
                {tpl.name}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("maintenance.asset")}</label>
          <select className="form-select" value={assetId} onChange={(e) => setAssetId(e.target.value)}>
            <option value="">{t("common.none")}</option>
            {assetsQuery.data?.data?.map((a) => (
              <option value={a.id} key={a.id}>
                {a.asset_code}
              </option>
            ))}
          </select>
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("maintenance.recurrenceRrule")}</label>
          <input className="form-control" value={rrule} onChange={(e) => setRrule(e.target.value)} />
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending || !code || !name || !templateId} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
