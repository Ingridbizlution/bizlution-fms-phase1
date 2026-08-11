import { useState } from "react";
import { useTranslation } from "react-i18next";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";
import { AssetReliabilityTab } from "./AssetReliabilityTab";
import { ExportCenterTab, type ExportJob } from "./ExportCenterTab";
import { GroupRollupTab } from "./GroupRollupTab";
import { PmComplianceTab } from "./PmComplianceTab";
import { ServiceVolumeTab } from "./ServiceVolumeTab";
import { SlaComplianceTab } from "./SlaComplianceTab";
import { SpaceUtilizationTab } from "./SpaceUtilizationTab";

const TABS = [
  { key: "sla", labelKey: "reports.tabSla" },
  { key: "pm", labelKey: "reports.tabPm" },
  { key: "rollup", labelKey: "reports.tabRollup" },
  { key: "reliability", labelKey: "reports.tabReliability" },
  { key: "space", labelKey: "reports.tabSpace" },
  { key: "service", labelKey: "reports.tabService" },
  { key: "exports", labelKey: "reports.tabExports" },
] as const;

export function ReportingPage() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<(typeof TABS)[number]["key"]>("sla");
  const [jobs, setJobs] = useState<ExportJob[]>([]);

  function queueExport(label: string, id: string) {
    setJobs((prev) => [...prev, { id, label, queuedAt: Date.now() }]);
  }

  return (
    <>
      <PageHeader title={t("reports.pageTitle")} />
      <PageBody>
        <div className="btn-group mb-3 flex-wrap">
          {TABS.map((tabItem) => (
            <button key={tabItem.key} type="button" className={`btn btn-sm ${tab === tabItem.key ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setTab(tabItem.key)}>
              {t(tabItem.labelKey)}
              {tabItem.key === "exports" && jobs.length > 0 && <span className="badge bg-secondary-lt ms-1">{jobs.length}</span>}
            </button>
          ))}
        </div>
        {tab === "sla" && <SlaComplianceTab onExportQueued={queueExport} />}
        {tab === "pm" && <PmComplianceTab onExportQueued={queueExport} />}
        {tab === "rollup" && <GroupRollupTab onExportQueued={queueExport} />}
        {tab === "reliability" && <AssetReliabilityTab onExportQueued={queueExport} />}
        {tab === "space" && <SpaceUtilizationTab onExportQueued={queueExport} />}
        {tab === "service" && <ServiceVolumeTab onExportQueued={queueExport} />}
        {tab === "exports" && <ExportCenterTab jobs={jobs} />}
      </PageBody>
    </>
  );
}
