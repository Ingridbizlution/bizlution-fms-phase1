import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useAuth } from "../../auth/AuthContext";
import { EmptyState } from "../../shell/EmptyState";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";
import { AlarmRulesTab } from "./AlarmRulesTab";
import { AlarmsTab } from "./AlarmsTab";
import { DevicesTab } from "./DevicesTab";
import { TelemetryTab } from "./TelemetryTab";

const TABS = [
  { key: "alarms", labelKey: "iot.tabAlarms" },
  { key: "devices", labelKey: "iot.tabDevices" },
  { key: "telemetry", labelKey: "iot.tabTelemetry" },
  { key: "rules", labelKey: "iot.tabRules" },
] as const;

export function IotPage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [tab, setTab] = useState<(typeof TABS)[number]["key"]>("alarms");

  return (
    <>
      <PageHeader title={t("iot.pageTitle")} />
      <PageBody>
        {!facilityId ? (
          <EmptyState title={t("iot.noFacilitySelected")} />
        ) : (
          <>
            <div className="btn-group mb-3">
              {TABS.map((tabItem) => (
                <button key={tabItem.key} type="button" className={`btn btn-sm ${tab === tabItem.key ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setTab(tabItem.key)}>
                  {t(tabItem.labelKey)}
                </button>
              ))}
            </div>
            {tab === "alarms" && <AlarmsTab facilityId={facilityId} />}
            {tab === "devices" && <DevicesTab facilityId={facilityId} />}
            {tab === "telemetry" && <TelemetryTab facilityId={facilityId} />}
            {tab === "rules" && <AlarmRulesTab facilityId={facilityId} />}
          </>
        )}
      </PageBody>
    </>
  );
}
