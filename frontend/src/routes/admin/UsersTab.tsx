import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { useTranslation } from "react-i18next";
import { assignRole, createUser, listRoleAssignments, listRoles, listSkills, listUserSkills, listUsers, revokeRoleAssignment, revokeUserSkill, setUserSkill, suspendUser, type UserCreate } from "../../api/admin";
import { ApiError } from "../../api/client";
import { useAuth } from "../../auth/AuthContext";
import { Can } from "../../auth/Can";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";
import { useCursorList } from "../../lib/useCursorList";

export function UsersTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [showForm, setShowForm] = useState(false);
  const [expandedUserId, setExpandedUserId] = useState<string | null>(null);
  const [expandedSkillsUserId, setExpandedSkillsUserId] = useState<string | null>(null);

  const { items, isLoading, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(["users"], (cursor) => listUsers(cursor));

  const suspendMutation = useMutation({
    mutationFn: (userId: string) => suspendUser(userId, "Suspended from the FMS console."),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["users"] }),
  });

  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("admin.tabUsers")}</h3>
        <Can permission="user:write">
          <button type="button" className="btn btn-sm btn-primary ms-auto" onClick={() => setShowForm((s) => !s)}>
            {showForm ? t("common.cancel") : t("admin.newUser")}
          </button>
        </Can>
      </div>
      {showForm && (
        <NewUserForm
          onDone={() => {
            setShowForm(false);
            queryClient.invalidateQueries({ queryKey: ["users"] });
          }}
        />
      )}
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("admin.colUser")}</th>
              <th>{t("common.type")}</th>
              <th>{t("common.status")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((u) => (
              <Fragment key={u.id}>
                <tr>
                  <td>
                    <code>{u.username}</code>
                    <div>{u.display_name}</div>
                  </td>
                  <td className="text-secondary">{u.user_type}</td>
                  <td>
                    <span className={`badge ${u.status === "ACTIVE" ? "bg-green-lt" : "bg-secondary-lt"}`}>{u.status}</span>
                  </td>
                  <td className="text-end">
                    <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setExpandedUserId(expandedUserId === u.id ? null : u.id!)}>
                      {t("admin.rolesButton")}
                    </button>
                    <button type="button" className="btn btn-sm btn-outline-secondary me-1" onClick={() => setExpandedSkillsUserId(expandedSkillsUserId === u.id ? null : u.id!)}>
                      {t("admin.skillsButton")}
                    </button>
                    <Can permission="user:write">
                      {u.status === "ACTIVE" && (
                        <button type="button" className="btn btn-sm btn-outline-danger" disabled={suspendMutation.isPending} onClick={() => suspendMutation.mutate(u.id!)}>
                          {t("admin.suspend")}
                        </button>
                      )}
                    </Can>
                  </td>
                </tr>
                {expandedUserId === u.id && (
                  <tr>
                    <td colSpan={4} className="bg-body-tertiary">
                      <RoleAssignments userId={u.id!} />
                    </td>
                  </tr>
                )}
                {expandedSkillsUserId === u.id && (
                  <tr>
                    <td colSpan={4} className="bg-body-tertiary">
                      <SkillAssignments userId={u.id!} />
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && items.length === 0 && <EmptyState title={t("admin.noUsers")} />}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}

function RoleAssignments({ userId }: { userId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { facilityId, currentUser } = useAuth();
  const [roleCode, setRoleCode] = useState("");

  const assignmentsQuery = useQuery({ queryKey: ["role-assignments", userId], queryFn: () => listRoleAssignments(userId) });
  const rolesQuery = useQuery({ queryKey: ["roles"], queryFn: listRoles });

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["role-assignments", userId] });
  const assignMutation = useMutation({
    mutationFn: () => assignRole(userId, { role_code: roleCode, scope_type: "FACILITY", scope_id: facilityId ?? undefined }),
    onSuccess: () => {
      setRoleCode("");
      invalidate();
    },
  });
  const revokeMutation = useMutation({ mutationFn: (id: string) => revokeRoleAssignment(id), onSuccess: invalidate });

  return (
    <div className="py-2">
      <div className="d-flex flex-wrap gap-1 mb-2">
        {assignmentsQuery.data?.items?.map((ra) => (
          <span className="badge bg-blue-lt d-inline-flex align-items-center gap-1" key={ra.id}>
            {ra.role_name ?? ra.role_code} @ {ra.scope_type}
            <Can permission="role:assign">
              <button type="button" className="btn-close btn-close-white" style={{ fontSize: 10 }} onClick={() => revokeMutation.mutate(ra.id!)} aria-label={t("admin.revoke")} />
            </Can>
          </span>
        ))}
        {!assignmentsQuery.data?.items?.length && <span className="text-secondary small">{t("admin.noRoleAssignments")}</span>}
      </div>
      <Can permission="role:assign">
        <div className="input-group input-group-sm" style={{ maxWidth: 360 }}>
          <select className="form-select" value={roleCode} onChange={(e) => setRoleCode(e.target.value)}>
            <option value="">{t("admin.assignRoleAt", { facility: currentUser?.accessible_facilities?.find((f) => f.id === facilityId)?.name })}</option>
            {rolesQuery.data?.items?.filter((r) => r.is_assignable !== false).map((r) => (
              <option value={r.code} key={r.code}>
                {r.name}
              </option>
            ))}
          </select>
          <button type="button" className="btn btn-primary" disabled={!roleCode || assignMutation.isPending} onClick={() => assignMutation.mutate()}>
            {t("admin.assign")}
          </button>
        </div>
      </Can>
    </div>
  );
}

const SKILL_STATUS_BADGE: Record<string, string> = {
  VALID: "bg-green-lt",
  EXPIRING: "bg-yellow-lt",
  EXPIRED: "bg-red-lt",
  NOT_APPLICABLE: "bg-secondary-lt",
};

function SkillAssignments({ userId }: { userId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [skillId, setSkillId] = useState("");
  const [level, setLevel] = useState(1);
  const [expiresAt, setExpiresAt] = useState("");

  const userSkillsQuery = useQuery({ queryKey: ["user-skills", userId], queryFn: () => listUserSkills(userId) });
  const skillsQuery = useQuery({ queryKey: ["skills"], queryFn: listSkills });

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["user-skills", userId] });
  const assignMutation = useMutation({
    mutationFn: () => setUserSkill(userId, skillId, { level, expires_at: expiresAt || undefined }),
    onSuccess: () => {
      setSkillId("");
      setExpiresAt("");
      invalidate();
    },
  });
  const revokeMutation = useMutation({ mutationFn: (id: string) => revokeUserSkill(userId, id), onSuccess: invalidate });

  const selectedSkill = skillsQuery.data?.items?.find((s) => s.id === skillId);

  return (
    <div className="py-2">
      <div className="d-flex flex-wrap gap-1 mb-2">
        {userSkillsQuery.data?.items?.map((us) => (
          <span className={`badge ${SKILL_STATUS_BADGE[us.status ?? ""] ?? "bg-secondary-lt"} d-inline-flex align-items-center gap-1`} key={us.skill_id}>
            {us.skill_name} L{us.level}
            <Can permission="team:write">
              <button
                type="button"
                className="btn-close"
                style={{ fontSize: 10 }}
                onClick={() => revokeMutation.mutate(us.skill_id!)}
                aria-label={t("admin.revoke")}
              />
            </Can>
          </span>
        ))}
        {!userSkillsQuery.data?.items?.length && <span className="text-secondary small">{t("admin.noSkillAssignments")}</span>}
      </div>
      <Can permission="team:write">
        <div className="d-flex flex-wrap gap-1 align-items-center">
          <select className="form-select form-select-sm" style={{ maxWidth: 220 }} value={skillId} onChange={(e) => setSkillId(e.target.value)}>
            <option value="">{t("admin.selectSkill")}</option>
            {skillsQuery.data?.items?.map((s) => (
              <option value={s.id} key={s.id}>
                {s.name}
              </option>
            ))}
          </select>
          <select className="form-select form-select-sm" style={{ maxWidth: 100 }} value={level} onChange={(e) => setLevel(Number(e.target.value))}>
            {[1, 2, 3, 4, 5].map((l) => (
              <option value={l} key={l}>
                L{l}
              </option>
            ))}
          </select>
          {selectedSkill?.requires_certification && (
            <input type="date" className="form-control form-control-sm" style={{ maxWidth: 160 }} value={expiresAt} onChange={(e) => setExpiresAt(e.target.value)} />
          )}
          <button
            type="button"
            className="btn btn-sm btn-primary"
            disabled={!skillId || (selectedSkill?.requires_certification && !expiresAt) || assignMutation.isPending}
            onClick={() => assignMutation.mutate()}
          >
            {t("admin.assign")}
          </button>
        </div>
      </Can>
    </div>
  );
}

function NewUserForm({ onDone }: { onDone: () => void }) {
  const { t } = useTranslation();
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [email, setEmail] = useState("");
  const [userType, setUserType] = useState<UserCreate["user_type"]>("EMPLOYEE");

  const mutation = useMutation({
    mutationFn: () => createUser({ username, display_name: displayName, email: email || undefined, user_type: userType }),
    onSuccess: onDone,
  });

  return (
    <div className="card-body border-bottom bg-body-tertiary">
      {mutation.isError && (
        <div className="alert alert-danger">{mutation.error instanceof ApiError ? mutation.error.problem.detail ?? mutation.error.message : t("admin.createUserError")}</div>
      )}
      <div className="row g-2 align-items-end">
        <div className="col-md-3">
          <label className="form-label">{t("admin.username")}</label>
          <input className="form-control" value={username} onChange={(e) => setUsername(e.target.value)} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("admin.displayName")}</label>
          <input className="form-control" value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
        </div>
        <div className="col-md-3">
          <label className="form-label">{t("admin.email")}</label>
          <input type="email" className="form-control" value={email} onChange={(e) => setEmail(e.target.value)} />
        </div>
        <div className="col-md-2">
          <label className="form-label">{t("common.type")}</label>
          <select className="form-select" value={userType} onChange={(e) => setUserType(e.target.value as UserCreate["user_type"])}>
            <option value="EMPLOYEE">{t("admin.userTypeEmployee")}</option>
            <option value="CONTRACTOR">{t("admin.userTypeContractor")}</option>
            <option value="VENDOR">{t("admin.userTypeVendor")}</option>
          </select>
        </div>
        <div className="col-md-1">
          <button type="button" className="btn btn-primary w-100" disabled={mutation.isPending || !username || !displayName} onClick={() => mutation.mutate()}>
            {t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
