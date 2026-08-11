import { apiFetch } from "./client";
import type { components } from "./schema";

export type Attachment = components["schemas"]["Attachment"];
export type AttachmentEntityType = "WORK_ORDER" | "ASSET" | "SPATIAL_NODE" | "RESERVATION";

export function listAttachments(entityType: AttachmentEntityType, entityId: string): Promise<{ data?: Attachment[] }> {
  return apiFetch(`/attachments?entity_type=${entityType}&entity_id=${entityId}`);
}

export function uploadAttachment(entityType: AttachmentEntityType, entityId: string, file: File, purpose?: string): Promise<Attachment> {
  const form = new FormData();
  form.set("entity_type", entityType);
  form.set("entity_id", entityId);
  if (purpose) form.set("purpose", purpose);
  form.set("file", file);
  return apiFetch("/attachments", { method: "POST", body: form });
}

export function deleteAttachment(attachmentId: string): Promise<void> {
  return apiFetch(`/attachments/${attachmentId}`, { method: "DELETE" });
}
