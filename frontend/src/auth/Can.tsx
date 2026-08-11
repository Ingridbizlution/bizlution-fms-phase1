import type { ReactNode } from "react";
import { useAuth } from "./AuthContext";
import { hasPermission, type PermissionScope } from "./permissions";

export function useCan(action: string, scope?: PermissionScope): boolean {
  const { currentUser } = useAuth();
  return hasPermission(currentUser?.permissions, action, scope);
}

interface CanProps {
  permission: string;
  scope?: PermissionScope;
  children: ReactNode;
  fallback?: ReactNode;
}

/** Gates UI on the flattened `permissions[]` from `/auth/me` — never re-derived from roles client-side. */
export function Can({ permission, scope, children, fallback = null }: CanProps) {
  const allowed = useCan(permission, scope);
  return <>{allowed ? children : fallback}</>;
}
