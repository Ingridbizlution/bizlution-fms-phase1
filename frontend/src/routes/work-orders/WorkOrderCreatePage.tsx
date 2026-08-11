import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useForm } from "react-hook-form";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { z } from "zod";
import { listAssets } from "../../api/assets";
import { ApiError } from "../../api/client";
import { createWorkOrder } from "../../api/workOrders";
import { useAuth } from "../../auth/AuthContext";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

const schema = z.object({
  title: z.string().min(1, "workOrders.create.required"),
  description: z.string().optional(),
  workOrderType: z.enum(["MAINTENANCE", "SERVICE", "INSPECTION", "CORRECTIVE", "PROJECT"]),
  priority: z.enum(["LOW", "MEDIUM", "HIGH", "URGENT", "CRITICAL"]),
  assetId: z.string().optional(),
});
type FormValues = z.infer<typeof schema>;

export function WorkOrderCreatePage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const assetsQuery = useQuery({
    queryKey: ["assets-picker", facilityId],
    queryFn: () => listAssets({ facilityId: facilityId ?? undefined, limit: 50 }),
    enabled: !!facilityId,
  });

  const {
    register,
    handleSubmit,
    formState: { errors },
  } = useForm<FormValues>({ resolver: zodResolver(schema), defaultValues: { workOrderType: "CORRECTIVE", priority: "MEDIUM" } });

  const mutation = useMutation({
    mutationFn: (values: FormValues) =>
      createWorkOrder({
        facility_id: facilityId!,
        work_order_type: values.workOrderType,
        title: values.title,
        description: values.description,
        asset_id: values.assetId || undefined,
        priority: values.priority,
        as_draft: false,
      }),
    onSuccess: (wo) => {
      queryClient.invalidateQueries({ queryKey: ["work-orders-table"] });
      queryClient.invalidateQueries({ queryKey: ["work-orders-kanban"] });
      navigate(`/work-orders/${wo.id}`);
    },
  });

  return (
    <>
      <PageHeader title={t("workOrders.create.title")} />
      <PageBody>
        <form className="card" style={{ maxWidth: 640 }} onSubmit={handleSubmit((v) => mutation.mutate(v))} noValidate>
          <div className="card-body">
            {mutation.isError && (
              <div className="alert alert-danger">
                {mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("workOrders.create.createError")}
              </div>
            )}

            <div className="mb-3">
              <label className="form-label" htmlFor="title">
                {t("workOrders.create.titleLabel")}
              </label>
              <input id="title" className={`form-control ${errors.title ? "is-invalid" : ""}`} {...register("title")} />
              {errors.title && <div className="invalid-feedback">{t(errors.title.message as string)}</div>}
            </div>

            <div className="mb-3">
              <label className="form-label" htmlFor="description">
                {t("workOrders.create.description")}
              </label>
              <textarea id="description" className="form-control" rows={3} {...register("description")} />
            </div>

            <div className="row">
              <div className="col-6 mb-3">
                <label className="form-label" htmlFor="workOrderType">
                  {t("workOrders.create.type")}
                </label>
                <select id="workOrderType" className="form-select" {...register("workOrderType")}>
                  <option value="CORRECTIVE">{t("workOrders.create.typeCorrective")}</option>
                  <option value="MAINTENANCE">{t("workOrders.create.typeMaintenance")}</option>
                  <option value="SERVICE">{t("workOrders.create.typeService")}</option>
                  <option value="INSPECTION">{t("workOrders.create.typeInspection")}</option>
                  <option value="PROJECT">{t("workOrders.create.typeProject")}</option>
                </select>
              </div>
              <div className="col-6 mb-3">
                <label className="form-label" htmlFor="priority">
                  {t("workOrders.create.priority")}
                </label>
                <select id="priority" className="form-select" {...register("priority")}>
                  <option value="LOW">{t("workOrders.create.priorityLow")}</option>
                  <option value="MEDIUM">{t("workOrders.create.priorityMedium")}</option>
                  <option value="HIGH">{t("workOrders.create.priorityHigh")}</option>
                  <option value="URGENT">{t("workOrders.create.priorityUrgent")}</option>
                  <option value="CRITICAL">{t("workOrders.create.priorityCritical")}</option>
                </select>
              </div>
            </div>

            <div className="mb-3">
              <label className="form-label" htmlFor="assetId">
                {t("workOrders.create.relatedAsset")}
              </label>
              <select id="assetId" className="form-select" {...register("assetId")}>
                <option value="">{t("workOrders.create.none")}</option>
                {assetsQuery.data?.data?.map((a) => (
                  <option value={a.id} key={a.id}>
                    {a.asset_code} — {a.name}
                  </option>
                ))}
              </select>
            </div>
          </div>
          <div className="card-footer text-end">
            <button type="submit" className="btn btn-primary" disabled={mutation.isPending}>
              {mutation.isPending ? t("workOrders.create.creating") : t("workOrders.create.createBtn")}
            </button>
          </div>
        </form>
      </PageBody>
    </>
  );
}
