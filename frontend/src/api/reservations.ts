import { apiFetch, newIdempotencyKey } from "./client";
import type { components } from "./schema";
import type { PagedEnvelope } from "../lib/useCursorList";

export type Reservation = components["schemas"]["Reservation"];
export type ReservationDetail = components["schemas"]["ReservationDetail"];
export type ReservationCreate = components["schemas"]["ReservationCreate"];
export type ReservationUpdate = components["schemas"]["ReservationUpdate"];
export type ResourceAvailability = components["schemas"]["ResourceAvailability"];

export function getAvailability(
  facilityId: string,
  params: { from: string; to: string; resourceIds?: string },
): Promise<{ data?: ResourceAvailability[] }> {
  const query = new URLSearchParams({ from: params.from, to: params.to });
  if (params.resourceIds) query.set("resource_ids", params.resourceIds);
  return apiFetch(`/facilities/${facilityId}/availability?${query}`);
}

export function createHold(body: { resource_id: string; start_at: string; end_at: string; ttl_seconds?: number }) {
  return apiFetch<{ hold_token?: string; expires_at?: string }>("/reservations/holds", {
    method: "POST",
    body,
    idempotencyKey: newIdempotencyKey(),
  });
}

export function releaseHold(holdToken: string) {
  return apiFetch<void>(`/reservations/holds/${holdToken}`, { method: "DELETE" });
}

export function createReservation(body: ReservationCreate): Promise<ReservationDetail> {
  return apiFetch<ReservationDetail>("/reservations", { method: "POST", body, idempotencyKey: newIdempotencyKey() });
}

export interface ListReservationsParams {
  facilityId?: string;
  status?: string;
  mine?: boolean;
  from?: string;
  to?: string;
  cursor?: string;
  limit?: number;
}

export function listReservations(params: ListReservationsParams): Promise<PagedEnvelope<Reservation>> {
  const query = new URLSearchParams();
  if (params.facilityId) query.set("facility_id", params.facilityId);
  if (params.status) query.set("status", params.status);
  if (params.mine) query.set("mine", "true");
  if (params.from) query.set("from", params.from);
  if (params.to) query.set("to", params.to);
  if (params.cursor) query.set("cursor", params.cursor);
  query.set("limit", String(params.limit ?? 50));
  return apiFetch<PagedEnvelope<Reservation>>(`/reservations?${query}`);
}

export function getReservation(reservationId: string): Promise<ReservationDetail> {
  return apiFetch<ReservationDetail>(`/reservations/${reservationId}`);
}

export function updateReservation(reservationId: string, patch: ReservationUpdate): Promise<ReservationDetail> {
  return apiFetch<ReservationDetail>(`/reservations/${reservationId}`, { method: "PATCH", body: patch });
}

export function cancelReservation(reservationId: string, reason?: string) {
  const query = reason ? `?reason=${encodeURIComponent(reason)}` : "";
  return apiFetch<void>(`/reservations/${reservationId}${query}`, { method: "DELETE" });
}

export function checkInReservation(reservationId: string) {
  return apiFetch<Reservation>(`/reservations/${reservationId}/check-in`, { method: "POST" });
}

export interface CheckOutResult {
  data?: { reservation_id?: string; status?: string; checked_in_at?: string; checked_out_at?: string };
  meta?: { used_minutes?: number; booked_minutes?: number; slot_released?: boolean; slot_released_by?: string };
}

export function checkOutReservation(reservationId: string): Promise<CheckOutResult> {
  return apiFetch<CheckOutResult>(`/reservations/${reservationId}/check-out`, { method: "POST" });
}

export function approveReservation(reservationId: string) {
  return apiFetch<Reservation>(`/reservations/${reservationId}/approve`, { method: "POST" });
}

export function rejectReservation(reservationId: string, reason: string) {
  return apiFetch<Reservation>(`/reservations/${reservationId}/reject`, { method: "POST", body: { reason } });
}

export interface CancelSeriesResult {
  data?: { recurrence_group_id?: string; cancelled?: number; skipped_past?: number; skipped_terminal?: number; total_in_series?: number };
}

export function cancelReservationSeries(recurrenceGroupId: string, reason?: string): Promise<CancelSeriesResult> {
  return apiFetch<CancelSeriesResult>(`/reservation-series/${recurrenceGroupId}`, {
    method: "DELETE",
    body: reason ? { reason } : undefined,
  });
}
