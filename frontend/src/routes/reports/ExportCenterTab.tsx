import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { getReportExport } from "../../api/reports";
import { EmptyState } from "../../shell/EmptyState";

export interface ExportJob {
  id: string;
  label: string;
  queuedAt: number;
}

function statusBadge(status: string | undefined): string {
  switch (status) {
    case "COMPLETED":
      return "bg-green-lt";
    case "FAILED":
      return "bg-red-lt";
    case "RUNNING":
      return "bg-yellow-lt";
    default:
      return "bg-blue-lt";
  }
}

function ExportRow({ job }: { job: ExportJob }) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ["report-export", job.id],
    queryFn: () => getReportExport(job.id),
    refetchInterval: (query) => (query.state.data?.status === "COMPLETED" || query.state.data?.status === "FAILED" ? false : 1500),
  });

  return (
    <tr>
      <td>{job.label}</td>
      <td className="text-secondary">{new Date(job.queuedAt).toLocaleTimeString()}</td>
      <td>
        <span className={`badge ${statusBadge(data?.status)}`}>{data?.status ?? "PENDING"}</span>
      </td>
      <td>{data?.row_count ?? "—"}</td>
      <td>
        {data?.status === "COMPLETED" && data.download_url && (
          <a href={data.download_url} className="btn btn-sm btn-outline-primary" target="_blank" rel="noreferrer">
            {t("reports.download")}
          </a>
        )}
        {data?.status === "FAILED" && <span className="text-danger small">{data.error}</span>}
      </td>
    </tr>
  );
}

export function ExportCenterTab({ jobs }: { jobs: ExportJob[] }) {
  const { t } = useTranslation();
  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title">{t("reports.exportJobsTitle")}</h3>
      </div>
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("reports.colReport")}</th>
              <th>{t("reports.colQueued")}</th>
              <th>{t("common.status")}</th>
              <th>{t("reports.colRows")}</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {jobs
              .slice()
              .reverse()
              .map((job) => (
                <ExportRow job={job} key={job.id} />
              ))}
          </tbody>
        </table>
      </div>
      {jobs.length === 0 && <EmptyState title={t("reports.noExportsQueued")} subtitle={t("reports.noExportsQueuedSubtitle")} />}
    </div>
  );
}
