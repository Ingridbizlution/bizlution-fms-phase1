import { apiFetch } from "./client";
import type { components } from "./schema";

export type Notification = components["schemas"]["Notification"];

export function listNotifications(unreadOnly = false): Promise<{ data: Notification[]; meta: { unread_count: number } }> {
  const query = unreadOnly ? "?unread_only=true" : "";
  return apiFetch(`/notifications${query}`);
}

export function markNotificationRead(notificationId: string): Promise<void> {
  return apiFetch(`/notifications/${notificationId}/read`, { method: "POST" });
}
