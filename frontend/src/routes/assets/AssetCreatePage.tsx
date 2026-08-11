import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useForm } from "react-hook-form";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { z } from "zod";
import { createAsset } from "../../api/assets";
import { ApiError } from "../../api/client";
import { useAuth } from "../../auth/AuthContext";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

const schema = z.object({
  categoryCode: z.string().min(1, "assets.create.required"),
  assetCode: z.string().min(1, "assets.create.required"),
  name: z.string().min(1, "assets.create.required"),
  criticality: z.enum(["LOW", "MEDIUM", "HIGH", "CRITICAL"]),
  status: z.string().min(1, "assets.create.required"),
});
type FormValues = z.infer<typeof schema>;

export function AssetCreatePage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const {
    register,
    handleSubmit,
    formState: { errors },
  } = useForm<FormValues>({ resolver: zodResolver(schema), defaultValues: { criticality: "MEDIUM", status: "ACTIVE" } });

  const mutation = useMutation({
    mutationFn: (values: FormValues) =>
      createAsset({
        facility_id: facilityId!,
        category_code: values.categoryCode,
        asset_code: values.assetCode,
        name: values.name,
        criticality: values.criticality,
        status: values.status,
      }),
    onSuccess: (asset) => {
      queryClient.invalidateQueries({ queryKey: ["assets"] });
      navigate(`/assets/${asset.id}`);
    },
  });

  return (
    <>
      <PageHeader title={t("assets.create.title")} />
      <PageBody>
        <form className="card" style={{ maxWidth: 640 }} onSubmit={handleSubmit((v) => mutation.mutate(v))} noValidate>
          <div className="card-body">
            {mutation.isError && (
              <div className="alert alert-danger">
                {mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("assets.create.createError")}
              </div>
            )}

            <div className="mb-3">
              <label className="form-label" htmlFor="assetCode">
                {t("assets.create.assetCode")}
              </label>
              <input id="assetCode" className={`form-control ${errors.assetCode ? "is-invalid" : ""}`} {...register("assetCode")} />
              {errors.assetCode && <div className="invalid-feedback">{t(errors.assetCode.message as string)}</div>}
            </div>

            <div className="mb-3">
              <label className="form-label" htmlFor="name">
                {t("assets.create.name")}
              </label>
              <input id="name" className={`form-control ${errors.name ? "is-invalid" : ""}`} {...register("name")} />
              {errors.name && <div className="invalid-feedback">{t(errors.name.message as string)}</div>}
            </div>

            <div className="mb-3">
              <label className="form-label" htmlFor="categoryCode">
                {t("assets.create.categoryCode")}
              </label>
              <input id="categoryCode" className={`form-control ${errors.categoryCode ? "is-invalid" : ""}`} placeholder={t("assets.create.categoryPlaceholder")} {...register("categoryCode")} />
              {errors.categoryCode && <div className="invalid-feedback">{t(errors.categoryCode.message as string)}</div>}
            </div>

            <div className="row">
              <div className="col-6 mb-3">
                <label className="form-label" htmlFor="criticality">
                  {t("assets.create.criticality")}
                </label>
                <select id="criticality" className="form-select" {...register("criticality")}>
                  <option value="LOW">{t("assets.create.low")}</option>
                  <option value="MEDIUM">{t("assets.create.medium")}</option>
                  <option value="HIGH">{t("assets.create.high")}</option>
                  <option value="CRITICAL">{t("assets.create.critical")}</option>
                </select>
              </div>
              <div className="col-6 mb-3">
                <label className="form-label" htmlFor="status">
                  {t("assets.create.status")}
                </label>
                <select id="status" className="form-select" {...register("status")}>
                  <option value="ACTIVE">{t("assets.statusActive")}</option>
                  <option value="DOWN">{t("assets.statusDown")}</option>
                  <option value="DEGRADED">{t("assets.statusDegraded")}</option>
                  <option value="MAINTENANCE">{t("assets.statusMaintenance")}</option>
                </select>
              </div>
            </div>
          </div>
          <div className="card-footer text-end">
            <button type="submit" className="btn btn-primary" disabled={mutation.isPending}>
              {mutation.isPending ? t("assets.create.creating") : t("assets.create.createBtn")}
            </button>
          </div>
        </form>
      </PageBody>
    </>
  );
}
