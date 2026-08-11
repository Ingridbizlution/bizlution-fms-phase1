import { zodResolver } from "@hookform/resolvers/zod";
import { useState } from "react";
import { useForm } from "react-hook-form";
import { useTranslation } from "react-i18next";
import { Navigate, useLocation } from "react-router-dom";
import { z } from "zod";
import { useAuth } from "../auth/AuthContext";

const schema = z.object({
  tenantCode: z.string().min(1, "login.validation.tenantCode"),
  username: z.string().min(1, "login.validation.username"),
  password: z.string().min(1, "login.validation.password"),
});
type FormValues = z.infer<typeof schema>;

const ERROR_KEYS: Record<string, string> = {
  UNAUTHENTICATED: "login.errors.unauthenticated",
  TOO_MANY_REQUESTS: "login.errors.tooManyRequests",
  TENANT_MISMATCH: "login.errors.tenantMismatch",
};

export function LoginPage() {
  const { t } = useTranslation();
  const { status, login } = useAuth();
  const location = useLocation();
  const [submitError, setSubmitError] = useState<string | null>(null);

  const {
    register,
    handleSubmit,
    formState: { errors, isSubmitting },
  } = useForm<FormValues>({ resolver: zodResolver(schema) });

  if (status === "authenticated") {
    const from = (location.state as { from?: Location })?.from?.pathname ?? "/";
    return <Navigate to={from} replace />;
  }

  const onSubmit = async (values: FormValues) => {
    setSubmitError(null);
    try {
      await login(values);
    } catch (err) {
      const code = (err as { code?: string }).code;
      setSubmitError(t((code && ERROR_KEYS[code]) || "login.errors.generic"));
    }
  };

  return (
    <div className="page page-center">
      <div className="container container-tight py-4">
        <div className="text-center mb-4">
          <span className="navbar-brand navbar-brand-autodark fs-1 fw-bold">Facility Management System</span>
        </div>
        <form className="card card-md" onSubmit={handleSubmit(onSubmit)} noValidate>
          <div className="card-body">
            <h2 className="h2 text-center mb-4">{t("login.title")}</h2>

            {submitError && <div className="alert alert-danger">{submitError}</div>}

            <div className="mb-3">
              <label className="form-label" htmlFor="tenantCode">
                {t("login.tenantCode")}
              </label>
              <input
                id="tenantCode"
                className={`form-control ${errors.tenantCode ? "is-invalid" : ""}`}
                placeholder={t("login.tenantCodePlaceholder")}
                autoComplete="organization"
                {...register("tenantCode")}
              />
              {errors.tenantCode && <div className="invalid-feedback">{t(errors.tenantCode.message as string)}</div>}
            </div>

            <div className="mb-3">
              <label className="form-label" htmlFor="username">
                {t("login.username")}
              </label>
              <input
                id="username"
                className={`form-control ${errors.username ? "is-invalid" : ""}`}
                autoComplete="username"
                {...register("username")}
              />
              {errors.username && <div className="invalid-feedback">{t(errors.username.message as string)}</div>}
            </div>

            <div className="mb-2">
              <label className="form-label" htmlFor="password">
                {t("login.password")}
              </label>
              <input
                id="password"
                type="password"
                className={`form-control ${errors.password ? "is-invalid" : ""}`}
                autoComplete="current-password"
                {...register("password")}
              />
              {errors.password && <div className="invalid-feedback">{t(errors.password.message as string)}</div>}
            </div>

            <div className="form-footer">
              <button type="submit" className="btn btn-primary w-100" disabled={isSubmitting}>
                {isSubmitting ? t("login.signingIn") : t("login.signIn")}
              </button>
            </div>
          </div>

          <div className="hr-text hr-text-spaceless">{t("login.or")}</div>

          <div className="card-body">
            <button type="button" className="btn w-100" disabled title={t("login.ssoDisabledTitle")}>
              {t("login.signInSso")}
            </button>
          </div>
        </form>

        {import.meta.env.DEV && (
          <div className="text-center text-secondary mt-3 small">
            Dev demo accounts (tenant <code>DEMO_GROUP</code>, password <code>Demo1234!</code>): <code>admin.chen</code>{" "}
            (full access), <code>user.huang</code> (requester — good for testing 403s).
          </div>
        )}
      </div>
    </div>
  );
}
