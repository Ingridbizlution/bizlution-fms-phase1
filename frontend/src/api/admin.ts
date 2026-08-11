import { apiFetch } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type User = components["schemas"]["User"];
export type UserCreate = components["schemas"]["UserCreate"];
export type Role = components["schemas"]["Role"];
export type Permission = components["schemas"]["Permission"];
export type RoleAssignment = components["schemas"]["RoleAssignment"];
export type AuditEntry = components["schemas"]["AuditEntry"];
export type IdentityProvider = components["schemas"]["IdentityProvider"];
export type NotificationTemplate = components["schemas"]["NotificationTemplate"];
export type WebhookSubscription = components["schemas"]["WebhookSubscription"];
export type Skill = components["schemas"]["Skill"];

// ---- Users ----
export function listUsers(cursor?: string): Promise<PagedEnvelope<User>> {
  const query = cursor ? `?cursor=${cursor}` : "";
  return apiFetch(`/users${query}`);
}

export function createUser(body: UserCreate): Promise<User> {
  return apiFetch("/users", { method: "POST", body });
}

export function suspendUser(userId: string, reason?: string): Promise<User> {
  return apiFetch(`/users/${userId}/suspend`, { method: "POST", body: { status: "SUSPENDED", reason } });
}

export function listRoleAssignments(userId: string): Promise<{ items?: RoleAssignment[] }> {
  return apiFetch(`/users/${userId}/role-assignments`);
}

export function assignRole(userId: string, body: { role_code: string; scope_type: string; scope_id?: string }): Promise<RoleAssignment> {
  return apiFetch(`/users/${userId}/role-assignments`, { method: "POST", body });
}

export function revokeRoleAssignment(assignmentId: string): Promise<void> {
  return apiFetch(`/role-assignments/${assignmentId}`, { method: "DELETE" });
}

// ---- Roles & permissions ----
export function listRoles(): Promise<{ items?: Role[] }> {
  return apiFetch("/roles");
}

export function createRole(body: { code: string; name: string; scope_level: string; permissions?: string[] }): Promise<Role> {
  return apiFetch("/roles", { method: "POST", body });
}

export interface RoleUpdate {
  name?: string;
  description?: string;
  is_assignable?: boolean;
  permissions?: string[];
}

export function updateRole(roleId: string, body: RoleUpdate): Promise<Role> {
  return apiFetch(`/roles/${roleId}`, { method: "PATCH", body });
}

export function deleteRole(roleId: string): Promise<void> {
  return apiFetch(`/roles/${roleId}`, { method: "DELETE" });
}

export function listPermissions(): Promise<{ items?: Permission[] }> {
  return apiFetch("/permissions");
}

// ---- Audit log ----
export function listAuditLog(cursor?: string): Promise<PagedEnvelope<AuditEntry>> {
  const query = cursor ? `?cursor=${cursor}` : "";
  return apiFetch(`/audit-log${query}`);
}

// ---- Identity providers ----
export function listIdentityProviders(): Promise<PagedEnvelope<IdentityProvider>> {
  return apiFetch("/identity-providers");
}

export function testIdentityProviderConnection(id: string): Promise<{ ok?: boolean; detail?: string }> {
  return apiFetch(`/identity-providers/${id}/test-connection`, { method: "POST" });
}

export function syncIdentityProvider(id: string): Promise<{ status?: string }> {
  return apiFetch(`/identity-providers/${id}/sync`, { method: "POST" });
}

export function deleteIdentityProvider(id: string): Promise<void> {
  return apiFetch(`/identity-providers/${id}`, { method: "DELETE" });
}

// ---- Directory groups & role mappings ----
export type DirectoryGroup = components["schemas"]["DirectoryGroup"];
export type DirectoryRoleMapping = components["schemas"]["DirectoryRoleMapping"];
export type DirectoryRoleMappingCreate = components["schemas"]["DirectoryRoleMappingCreate"];

export function listDirectoryGroups(identityProviderId: string): Promise<{ data: DirectoryGroup[] }> {
  return apiFetch(`/directory-groups?identity_provider_id=${identityProviderId}`);
}

export function listDirectoryRoleMappings(): Promise<{ items?: DirectoryRoleMapping[] }> {
  return apiFetch("/directory-role-mappings");
}

export function createDirectoryRoleMapping(body: DirectoryRoleMappingCreate): Promise<DirectoryRoleMapping> {
  return apiFetch("/directory-role-mappings", { method: "POST", body });
}

export function deleteDirectoryRoleMapping(id: string): Promise<{ deleted?: boolean; orphaned_assignments?: number }> {
  return apiFetch(`/directory-role-mappings/${id}`, { method: "DELETE" });
}

// ---- Notification templates ----
export function listNotificationTemplates(): Promise<{ data?: NotificationTemplate[] }> {
  return apiFetch("/notification-templates");
}

export function createNotificationTemplate(body: { code: string; channel: string; locale: string; subject_template?: string; body_template: string }): Promise<NotificationTemplate> {
  return apiFetch("/notification-templates", { method: "POST", body });
}

export interface NotificationTemplateUpdate {
  subject_template?: string;
  body_template?: string;
  is_active?: boolean;
}

export function updateNotificationTemplate(templateId: string, body: NotificationTemplateUpdate): Promise<NotificationTemplate> {
  return apiFetch(`/notification-templates/${templateId}`, { method: "PATCH", body });
}

export function deleteNotificationTemplate(templateId: string): Promise<void> {
  return apiFetch(`/notification-templates/${templateId}`, { method: "DELETE" });
}

// ---- Webhooks ----
export function listWebhooks(): Promise<{ data: WebhookSubscription[] }> {
  return apiFetch("/webhooks");
}

export function upsertWebhook(body: { url: string; event_types: string[]; description?: string; is_active?: boolean }): Promise<{ data: WebhookSubscription; signing_secret?: string }> {
  return apiFetch("/webhooks", { method: "POST", body });
}

// ---- Skills ----
export function listSkills(): Promise<{ items?: Skill[] }> {
  return apiFetch("/skills");
}

export function createSkill(body: { code: string; name: string; domain?: string; requires_certification?: boolean }): Promise<Skill> {
  return apiFetch("/skills", { method: "POST", body });
}

export interface SkillUpdate {
  name?: string;
  domain?: string;
  requires_certification?: boolean;
  reminder_days_before?: number;
}

export function updateSkill(skillId: string, body: SkillUpdate): Promise<Skill> {
  return apiFetch(`/skills/${skillId}`, { method: "PATCH", body });
}

export function deleteSkill(skillId: string): Promise<void> {
  return apiFetch(`/skills/${skillId}`, { method: "DELETE" });
}

export type UserSkill = components["schemas"]["UserSkill"];

export function listUserSkills(userId: string): Promise<{ items?: UserSkill[] }> {
  return apiFetch(`/users/${userId}/skills`);
}

export interface UserSkillUpsert {
  level?: number;
  certified_at?: string;
  expires_at?: string;
  certificate_no?: string;
}

export function setUserSkill(userId: string, skillId: string, body: UserSkillUpsert): Promise<UserSkill> {
  return apiFetch(`/users/${userId}/skills/${skillId}`, { method: "PUT", body });
}

export function revokeUserSkill(userId: string, skillId: string): Promise<void> {
  return apiFetch(`/users/${userId}/skills/${skillId}`, { method: "DELETE" });
}
