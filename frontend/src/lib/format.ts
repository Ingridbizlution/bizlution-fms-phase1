/** "RESOLUTION_BREACHED" -> "RESOLUTION BREACHED" — readable without changing
 *  the all-caps convention badges already use for single-word codes. */
export function humanizeEnum(value: string | undefined | null): string {
  if (!value) return "";
  return value.replaceAll("_", " ");
}
