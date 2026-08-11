import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useAuth } from "../../auth/AuthContext";
import { EmptyState } from "../../shell/EmptyState";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";
import { OccurrencesTab } from "./OccurrencesTab";
import { PlansTab } from "./PlansTab";
import { TemplatesTab } from "./TemplatesTab";

const TABS = [
  { key: "plans", labelKey: "maintenance.tabPlans" },
  { key: "occurrences", labelKey: "maintenance.tabOccurrences" },
  { key: "templates", labelKey: "maintenance.tabTemplates" },
] as const;

export function MaintenancePage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [tab, setTab] = useState<(typeof TABS)[number]["key"]>("plans");

  return (
    <>
      <PageHeader title={t("maintenance.pageTitle")} />
      <PageBody>
        {!facilityId ? (
          <EmptyState title={t("maintenance.noFacilitySelected")} />
        ) : (
          <>
            <div className="btn-group mb-3">
              {TABS.map((tabItem) => (
                <button key={tabItem.key} type="button" className={`btn btn-sm ${tab === tabItem.key ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setTab(tabItem.key)}>
                  {t(tabItem.labelKey)}
                </button>
              ))}
            </div>
            {tab === "plans" && <PlansTab facilityId={facilityId} />}
            {tab === "occurrences" && <OccurrencesTab facilityId={facilityId} />}
            {tab === "templates" && <TemplatesTab />}
          </>
        )}
      </PageBody>
    </>
  );
}
