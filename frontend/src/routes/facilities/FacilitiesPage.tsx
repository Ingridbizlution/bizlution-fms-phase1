import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useAuth } from "../../auth/AuthContext";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";
import { EmptyState } from "../../shell/EmptyState";
import { BimModelsTab } from "./BimModelsTab";
import { BlackoutsTab } from "./BlackoutsTab";
import { BookableResourcesTab } from "./BookableResourcesTab";
import { CalendarIntegrationsTab } from "./CalendarIntegrationsTab";
import { FloorPlan3DTab } from "./FloorPlan3DTab";
import { FloorViewTab } from "./FloorViewTab";
import { NodesTab } from "./NodesTab";

const TABS = [
  { key: "floor-view", labelKey: "facilities.tabFloorView" },
  { key: "floor-plan-3d", labelKey: "facilities.tabFloorPlan3D" },
  { key: "nodes", labelKey: "facilities.tabNodes" },
  { key: "bim", labelKey: "facilities.tabBim" },
  { key: "bookable-resources", labelKey: "facilities.tabBookableResources" },
  { key: "blackouts", labelKey: "facilities.tabBlackouts" },
  { key: "calendar", labelKey: "facilities.tabCalendar" },
] as const;

export function FacilitiesPage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [tab, setTab] = useState<(typeof TABS)[number]["key"]>("floor-view");

  return (
    <>
      <PageHeader title={t("facilities.pageTitle")} />
      <PageBody>
        {!facilityId ? (
          <EmptyState title={t("facilities.noFacilitySelected")} />
        ) : (
          <>
            <div className="btn-group mb-3">
              {TABS.map((tabItem) => (
                <button key={tabItem.key} type="button" className={`btn btn-sm ${tab === tabItem.key ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setTab(tabItem.key)}>
                  {t(tabItem.labelKey)}
                </button>
              ))}
            </div>
            {tab === "floor-view" && <FloorViewTab facilityId={facilityId} />}
            {tab === "floor-plan-3d" && <FloorPlan3DTab facilityId={facilityId} />}
            {tab === "nodes" && <NodesTab facilityId={facilityId} />}
            {tab === "bim" && <BimModelsTab facilityId={facilityId} />}
            {tab === "bookable-resources" && <BookableResourcesTab facilityId={facilityId} />}
            {tab === "blackouts" && <BlackoutsTab facilityId={facilityId} />}
            {tab === "calendar" && <CalendarIntegrationsTab facilityId={facilityId} />}
          </>
        )}
      </PageBody>
    </>
  );
}
