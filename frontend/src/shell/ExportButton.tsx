import { useMutation } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { exportReport, type DateRange, type ReportCode } from "../api/reports";

export function ExportButton({
  code,
  range,
  groupBy,
  facilityId,
  onQueued,
}: {
  code: ReportCode;
  range: DateRange;
  groupBy?: string;
  facilityId?: string;
  onQueued: (exportId: string) => void;
}) {
  const { t } = useTranslation();
  const mutation = useMutation({
    mutationFn: () => exportReport(code, { ...range, format: "csv", groupBy, facilityId }),
    onSuccess: (job) => job.id && onQueued(job.id),
  });

  return (
    <button type="button" className="btn btn-sm btn-outline-secondary" disabled={mutation.isPending} onClick={() => mutation.mutate()}>
      {mutation.isPending ? t("common.queuing") : mutation.isSuccess ? t("common.queued") : t("common.exportCsv")}
    </button>
  );
}
