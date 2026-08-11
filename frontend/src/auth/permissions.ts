/**
 * Entries look like `work_order:assign@FACILITY:cccccccc-0000-4000-8000-000000000001`
 * or `tenant:read` with no scope at all (tenant-wide permissions carry no `@`).
 */
export interface PermissionScope {
  type: string;
  id?: string | null;
}

export function hasPermission(
  permissions: string[] | undefined,
  action: string,
  scope?: PermissionScope,
): boolean {
  if (!permissions) return false;
  return permissions.some((entry) => {
    const [perm, scopePart] = entry.split("@");
    if (perm !== action) return false;
    if (!scope) return true;
    if (!scopePart) return false;
    const [scopeType, scopeId] = scopePart.split(":");
    if (scopeType !== scope.type) return false;
    return !scope.id || scopeId === scope.id;
  });
}
