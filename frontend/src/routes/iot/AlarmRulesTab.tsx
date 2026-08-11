import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { ApiError } from "../../api/client";
import { createAlarmRule, deleteAlarmRule, dryRunAlarmRule, listAlarmRules, updateAlarmRule, type AlarmRule } from "../../api/iot";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";

export function AlarmRulesTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<Record<string, string>>({});

  const rulesQuery = useQuery({ queryKey: ["alarm-rules", facilityId], queryFn: () => listAlarmRules(facilityId) });

  function invalidate() {
    return queryClient.invalidateQueries({ queryKey: ["alarm-rules", facilityId] });
  }

  const testMutation = useMutation({
    mutationFn: (ruleId: string) => dryRunAlarmRule(ruleId),
    onSuccess: (res, ruleId) => setTestResult((prev) => ({ ...prev, [ruleId]: t("iot.wouldHaveFired", { count: res.data?.would_have_fired ?? 0 }) })),
  });

  const toggleActiveMutation = useMutation({
    mutationFn: (rule: AlarmRule) => updateAlarmRule(rule.id!, { is_active: !rule.is_active }),
    onSuccess: invalidate,
  });

  const deleteMutation = useMutation({
    mutationFn: (id: string) => deleteAlarmRule(id),
    onSuccess: invalidate,
    onError: (err) => setRowError(err instanceof ApiError ? err.problem.detail ?? err.message : t("iot.deleteRuleError")),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("iot.tabRules")}</h3>
        <Can permission="alarm_rule:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("iot.newRule")}
          </button>
        </Can>
      </div>
      {showForm && (
        <NewRuleForm
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
              <th>{t("iot.colRule")}</th>
              <th>{t("iot.colPoint")}</th>
              <th>{t("iot.colCondition")}</th>
              <th>{t("iot.colSeverity")}</th>
              <th>{t("iot.colCovers")}</th>
              <th>{t("common.status")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {rulesQuery.data?.data?.map((rule) => (
              <Fragment key={rule.id}>
                <tr>
                  <td>
                    <code>{rule.code}</code>
                    <div>{rule.name}</div>
                    {testResult[rule.id!] && <div className="text-secondary small">{testResult[rule.id!]}</div>}
                  </td>
                  <td className="text-secondary">{rule.point_code ?? "—"}</td>
                  <td>
                    {(rule.condition as { op?: string; value?: number })?.op} {(rule.condition as { op?: string; value?: number })?.value}
                  </td>
                  <td>
                    <span className={`badge ${rule.severity === "CRITICAL" ? "bg-red-lt" : "bg-yellow-lt"}`}>{rule.severity}</span>
                  </td>
                  <td>
                    {rule.covered_point_count === 0 ? <span className="badge bg-red-lt">{t("iot.zeroPoints")}</span> : rule.covered_point_count}
                  </td>
                  <td>
                    <span className={`badge ${rule.is_active ? "bg-green-lt" : "bg-secondary-lt"}`}>
                      {rule.is_active ? t("maintenance.active") : t("maintenance.inactive")}
                    </span>
                  </td>
                  <td className="text-end">
                    <button type="button" className="btn btn-sm btn-outline-secondary me-1" disabled={testMutation.isPending} onClick={() => testMutation.mutate(rule.id!)}>
                      {t("iot.test")}
                    </button>
                    <Can permission="alarm_rule:write">
                      <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setEditingId(editingId === rule.id ? null : rule.id!)}>
                        {t("common.edit")}
                      </button>
                      <button
                        type="button"
                        className={`btn btn-sm me-1 ${rule.is_active ? "btn-outline-danger" : "btn-outline-success"}`}
                        disabled={toggleActiveMutation.isPending}
                        onClick={() => toggleActiveMutation.mutate(rule)}
                      >
                        {rule.is_active ? t("maintenance.deactivate") : t("maintenance.activate")}
                      </button>
                      <button
                        type="button"
                        className="btn btn-sm btn-outline-danger"
                        disabled={deleteMutation.isPending}
                        title={t("iot.deleteRuleBlockedHint")}
                        onClick={() => {
                          if (window.confirm(t("iot.confirmDeleteRule", { name: rule.name }))) deleteMutation.mutate(rule.id!);
                        }}
                      >
                        {t("common.delete")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {editingId === rule.id && (
                  <tr>
                    <td colSpan={7} className="bg-body-tertiary">
                      <EditRuleForm
                        rule={rule}
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
      {!rulesQuery.isLoading && rulesQuery.data?.data?.length === 0 && <EmptyState title={t("iot.noAlarmRules")} />}
    </div>
  );
}

function EditRuleForm({ rule, onDone }: { rule: AlarmRule; onDone: () => void }) {
  const { t } = useTranslation();
  const [name, setName] = useState(rule.name ?? "");
  const condition = rule.condition as { op?: string; value?: number };
  const [op, setOp] = useState(condition?.op ?? ">");
  const [value, setValue] = useState(condition?.value ?? 0);
  const [severity, setSeverity] = useState<string>(rule.severity ?? "WARNING");

  const mutation = useMutation({
    mutationFn: () => updateAlarmRule(rule.id!, { name, condition: { op, value }, severity }),
    onSuccess: onDone,
  });

  return (
    <div>
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("iot.updateRuleError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-3">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-1">
          <label className="form-label">{t("iot.op")}</label>
          <select className="form-select" value={op} onChange={(e) => setOp(e.target.value)}>
            <option value=">">&gt;</option>
            <option value="<">&lt;</option>
            <option value=">=">&ge;</option>
            <option value="<=">&le;</option>
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("iot.value")}</label>
          <input type="number" className="form-control" value={value} onChange={(e) => setValue(Number(e.target.value))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("iot.severity")}</label>
          <select className="form-select" value={severity} onChange={(e) => setSeverity(e.target.value)}>
            <option value="INFO">{t("iot.severityInfo")}</option>
            <option value="WARNING">{t("iot.severityWarning")}</option>
            <option value="MINOR">{t("iot.severityMinor")}</option>
            <option value="MAJOR">{t("iot.severityMajor")}</option>
            <option value="CRITICAL">{t("iot.severityCritical")}</option>
          </select>
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

function NewRuleForm({ facilityId, onDone }: { facilityId: string; onDone: () => void }) {
  const { t } = useTranslation();
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [pointCode, setPointCode] = useState("");
  const [op, setOp] = useState(">");
  const [value, setValue] = useState(28);
  const [severity, setSeverity] = useState("WARNING");

  const mutation = useMutation({
    mutationFn: () => createAlarmRule({ facility_id: facilityId, code, name, point_code: pointCode, condition: { op, value }, severity }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("iot.createRuleError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-2">
          <label className="form-label">{t("iot.code")}</label>
          <input className="form-control" value={code} onChange={(e) => setCode(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("common.name")}</label>
          <input className="form-control" value={name} onChange={(e) => setName(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("iot.pointCode")}</label>
          <input className="form-control" placeholder={t("iot.pointCodePlaceholder")} value={pointCode} onChange={(e) => setPointCode(e.target.value)} />
        </div>
        <div className="col-md-1">
          <label className="form-label">{t("iot.op")}</label>
          <select className="form-select" value={op} onChange={(e) => setOp(e.target.value)}>
            <option value=">">&gt;</option>
            <option value="<">&lt;</option>
            <option value=">=">&ge;</option>
            <option value="<=">&le;</option>
          </select>
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("iot.value")}</label>
          <input type="number" className="form-control" value={value} onChange={(e) => setValue(Number(e.target.value))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("iot.severity")}</label>
          <select className="form-select" value={severity} onChange={(e) => setSeverity(e.target.value)}>
            <option value="INFO">{t("iot.severityInfo")}</option>
            <option value="WARNING">{t("iot.severityWarning")}</option>
            <option value="MINOR">{t("iot.severityMinor")}</option>
            <option value="MAJOR">{t("iot.severityMajor")}</option>
            <option value="CRITICAL">{t("iot.severityCritical")}</option>
          </select>
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending || !code || !name || !pointCode} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
