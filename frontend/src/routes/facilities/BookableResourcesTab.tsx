import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { listBookableResources, updateBookableResource, type BookableResource, type BookableResourcePatch } from "../../api/bookableResources";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { humanizeEnum } from "../../lib/format";
import { EmptyState } from "../../shell/EmptyState";

const WEEKDAYS: { key: string; labelKey: string }[] = [
  { key: "mon", labelKey: "facilities.mon" },
  { key: "tue", labelKey: "facilities.tue" },
  { key: "wed", labelKey: "facilities.wed" },
  { key: "thu", labelKey: "facilities.thu" },
  { key: "fri", labelKey: "facilities.fri" },
  { key: "sat", labelKey: "facilities.sat" },
  { key: "sun", labelKey: "facilities.sun" },
];

export function BookableResourcesTab({ facilityId }: { facilityId: string }) {
  const { t } = useTranslation();
  const [editingId, setEditingId] = useState<string | null>(null);

  const { data, isLoading, isError } = useQuery({
    queryKey: ["bookable-resources", facilityId],
    queryFn: () => listBookableResources(facilityId, { includeUnbookable: true }),
  });

  const resources = data?.data ?? [];

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("facilities.tabBookableResources")}</h3>
        <span className="text-secondary ms-2">{t("facilities.bookableResourcesSubtitle")}</span>
      </div>
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("facilities.colResource")}</th>
              <th>{t("facilities.colType")}</th>
              <th>{t("facilities.colCapacity")}</th>
              <th>{t("facilities.colDuration")}</th>
              <th>{t("facilities.colApproval")}</th>
              <th>{t("facilities.colBookable")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {resources.map((r) => (
              <Fragment key={r.id}>
                <tr>
                  <td>{r.display_name ?? <span className="text-secondary">{t("facilities.unnamed")}</span>}</td>
                  <td className="text-secondary">{humanizeEnum(r.resource_type)}</td>
                  <td>{r.capacity}</td>
                  <td className="text-secondary">
                    {r.min_duration_minutes}–{r.max_duration_minutes} min
                  </td>
                  <td>{r.requires_approval ? <span className="badge bg-yellow-lt">{t("facilities.requiresApproval")}</span> : <span className="text-secondary">{t("facilities.auto")}</span>}</td>
                  <td>{r.is_bookable ? <span className="badge bg-green-lt">{t("facilities.bookableBadge")}</span> : <span className="badge bg-red-lt">{t("facilities.disabledBadge")}</span>}</td>
                  <td>
                    <Can permission="bookable_resource:write">
                      <button type="button" className="btn btn-sm btn-outline-primary" onClick={() => setEditingId(editingId === r.id ? null : r.id!)}>
                        {editingId === r.id ? t("common.close") : t("common.edit")}
                      </button>
                    </Can>
                  </td>
                </tr>
                {editingId === r.id && (
                  <tr>
                    <td colSpan={7} className="p-0">
                      <EditResourceForm resource={r} facilityId={facilityId} onDone={() => setEditingId(null)} />
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && resources.length === 0 && <EmptyState title={t("facilities.noBookableResources")} subtitle={t("facilities.noBookableResourcesSubtitle")} />}
      {isError && <div className="alert alert-danger m-3">{t("facilities.loadBookableResourcesError")}</div>}
    </div>
  );
}

function EditResourceForm({ resource, facilityId, onDone }: { resource: BookableResource; facilityId: string; onDone: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [form, setForm] = useState({
    capacity: resource.capacity ?? 1,
    min_duration_minutes: resource.min_duration_minutes ?? 15,
    max_duration_minutes: resource.max_duration_minutes ?? 480,
    slot_granularity_minutes: resource.slot_granularity_minutes ?? 15,
    buffer_before_minutes: resource.buffer_before_minutes ?? 0,
    buffer_after_minutes: resource.buffer_after_minutes ?? 0,
    advance_booking_days: resource.advance_booking_days ?? 30,
    min_notice_minutes: resource.min_notice_minutes ?? 0,
    max_active_per_user: resource.max_active_per_user ?? null,
    auto_release_minutes: resource.auto_release_minutes ?? null,
    approver_role_code: resource.approver_role_code ?? "",
    requires_approval: resource.requires_approval ?? false,
    requires_check_in: resource.requires_check_in ?? false,
    is_bookable: resource.is_bookable ?? true,
  });
  const openingHours = (resource.opening_hours as unknown as Record<string, [string, string][]>) ?? {};
  const [hours, setHours] = useState<Record<string, { from: string; to: string } | null>>(
    Object.fromEntries(WEEKDAYS.map((d) => [d.key, openingHours[d.key]?.[0] ? { from: openingHours[d.key][0][0], to: openingHours[d.key][0][1] } : null])),
  );

  const mutation = useMutation({
    mutationFn: () => {
      const patch: BookableResourcePatch = {
        capacity: form.capacity,
        min_duration_minutes: form.min_duration_minutes,
        max_duration_minutes: form.max_duration_minutes,
        slot_granularity_minutes: form.slot_granularity_minutes,
        buffer_before_minutes: form.buffer_before_minutes,
        buffer_after_minutes: form.buffer_after_minutes,
        advance_booking_days: form.advance_booking_days,
        min_notice_minutes: form.min_notice_minutes,
        max_active_per_user: form.max_active_per_user,
        auto_release_minutes: form.auto_release_minutes,
        approver_role_code: form.approver_role_code || null,
        requires_approval: form.requires_approval,
        requires_check_in: form.requires_check_in,
        is_bookable: form.is_bookable,
        opening_hours: Object.fromEntries(Object.entries(hours).filter(([, v]) => v).map(([k, v]) => [k, [[v!.from, v!.to]]])),
      };
      return updateBookableResource(resource.id!, patch);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["bookable-resources", facilityId] });
      onDone();
    },
  });

  return (
    <div className="card-body bg-body-tertiary border-top border-bottom">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("facilities.saveRulesError")}</div>
      )}
      <div className="row g-3">
        <div className="col-md-2">
          <label className="form-label">{t("facilities.capacity")}</label>
          <input type="number" min={1} className="form-control" value={form.capacity} onChange={(e) => setForm((f) => ({ ...f, capacity: Number(e.target.value) }))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.minDuration")}</label>
          <input type="number" min={1} className="form-control" value={form.min_duration_minutes} onChange={(e) => setForm((f) => ({ ...f, min_duration_minutes: Number(e.target.value) }))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.maxDuration")}</label>
          <input type="number" min={1} className="form-control" value={form.max_duration_minutes} onChange={(e) => setForm((f) => ({ ...f, max_duration_minutes: Number(e.target.value) }))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.slotGranularity")}</label>
          <input
            type="number"
            min={1}
            className="form-control"
            value={form.slot_granularity_minutes}
            onChange={(e) => setForm((f) => ({ ...f, slot_granularity_minutes: Number(e.target.value) }))}
          />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.bufferBefore")}</label>
          <input type="number" min={0} className="form-control" value={form.buffer_before_minutes} onChange={(e) => setForm((f) => ({ ...f, buffer_before_minutes: Number(e.target.value) }))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.bufferAfter")}</label>
          <input type="number" min={0} className="form-control" value={form.buffer_after_minutes} onChange={(e) => setForm((f) => ({ ...f, buffer_after_minutes: Number(e.target.value) }))} />
        </div>

        <div className="col-md-2">
          <label className="form-label">{t("facilities.advanceBooking")}</label>
          <input type="number" min={0} className="form-control" value={form.advance_booking_days} onChange={(e) => setForm((f) => ({ ...f, advance_booking_days: Number(e.target.value) }))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.minNotice")}</label>
          <input type="number" min={0} className="form-control" value={form.min_notice_minutes} onChange={(e) => setForm((f) => ({ ...f, min_notice_minutes: Number(e.target.value) }))} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.maxActivePerUser")}</label>
          <input
            type="number"
            min={0}
            className="form-control"
            value={form.max_active_per_user ?? ""}
            placeholder={t("facilities.unlimited")}
            onChange={(e) => setForm((f) => ({ ...f, max_active_per_user: e.target.value === "" ? null : Number(e.target.value) }))}
          />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.autoRelease")}</label>
          <input
            type="number"
            min={0}
            className="form-control"
            value={form.auto_release_minutes ?? ""}
            placeholder={t("facilities.never")}
            onChange={(e) => setForm((f) => ({ ...f, auto_release_minutes: e.target.value === "" ? null : Number(e.target.value) }))}
          />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("facilities.approverRoleCode")}</label>
          <input
            className="form-control"
            value={form.approver_role_code}
            placeholder={t("facilities.approverRoleCodePlaceholder")}
            onChange={(e) => setForm((f) => ({ ...f, approver_role_code: e.target.value }))}
          />
        </div>
        <div className="col-md-4 d-flex align-items-end gap-3">
          <label className="form-check">
            <input type="checkbox" className="form-check-input" checked={form.is_bookable} onChange={(e) => setForm((f) => ({ ...f, is_bookable: e.target.checked }))} />
            <span className="form-check-label">{t("facilities.bookable")}</span>
          </label>
          <label className="form-check">
            <input type="checkbox" className="form-check-input" checked={form.requires_approval} onChange={(e) => setForm((f) => ({ ...f, requires_approval: e.target.checked }))} />
            <span className="form-check-label">{t("facilities.requiresApprovalCheckbox")}</span>
          </label>
          <label className="form-check">
            <input type="checkbox" className="form-check-input" checked={form.requires_check_in} onChange={(e) => setForm((f) => ({ ...f, requires_check_in: e.target.checked }))} />
            <span className="form-check-label">{t("facilities.requiresCheckIn")}</span>
          </label>
        </div>
      </div>

      <hr />
      <label className="form-label">{t("facilities.openingHours")}</label>
      <div className="d-flex flex-wrap gap-3 mb-3">
        {WEEKDAYS.map((d) => (
          <div key={d.key} className="d-flex align-items-center gap-1">
            <span className="text-secondary small" style={{ width: 32 }}>
              {t(d.labelKey)}
            </span>
            <input
              type="time"
              className="form-control form-control-sm"
              style={{ width: 110 }}
              value={hours[d.key]?.from ?? ""}
              onChange={(e) => setHours((h) => ({ ...h, [d.key]: e.target.value ? { from: e.target.value, to: h[d.key]?.to ?? "18:00" } : null }))}
            />
            <span className="text-secondary">–</span>
            <input
              type="time"
              className="form-control form-control-sm"
              style={{ width: 110 }}
              value={hours[d.key]?.to ?? ""}
              onChange={(e) => setHours((h) => ({ ...h, [d.key]: e.target.value ? { from: h[d.key]?.from ?? "08:00", to: e.target.value } : null }))}
            />
          </div>
        ))}
      </div>

      <div className="d-flex gap-2">
        <button type="button" className="btn btn-primary" disabled={mutation.isPending} onClick={() => mutation.mutate()}>
          {t("facilities.saveRules")}
        </button>
        <button type="button" className="btn btn-link" onClick={onDone}>
          {t("common.cancel")}
        </button>
      </div>
    </div>
  );
}
