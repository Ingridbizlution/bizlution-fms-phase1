import { useState } from "react";
import { useTranslation } from "react-i18next";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";
import { AuditLogTab } from "./AuditLogTab";
import { FacilitiesTab } from "./FacilitiesTab";
import { IdentityProvidersTab } from "./IdentityProvidersTab";
import { NotificationTemplatesTab } from "./NotificationTemplatesTab";
import { OrganizationsTab } from "./OrganizationsTab";
import { RolesTab } from "./RolesTab";
import { SkillsTab } from "./SkillsTab";
import { TenantTab } from "./TenantTab";
import { UsersTab } from "./UsersTab";
import { WebhooksTab } from "./WebhooksTab";

const TABS = [
  { key: "users", labelKey: "admin.tabUsers" },
  { key: "roles", labelKey: "admin.tabRoles" },
  { key: "facilities", labelKey: "admin.tabFacilities" },
  { key: "organizations", labelKey: "admin.tabOrganizations" },
  { key: "identity", labelKey: "admin.tabIdentity" },
  { key: "audit", labelKey: "admin.tabAudit" },
  { key: "notifications", labelKey: "admin.tabNotifications" },
  { key: "webhooks", labelKey: "admin.tabWebhooks" },
  { key: "skills", labelKey: "admin.tabSkills" },
  { key: "tenant", labelKey: "admin.tabTenant" },
] as const;

export function AdminPage() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<(typeof TABS)[number]["key"]>("users");

  return (
    <>
      <PageHeader title={t("admin.pageTitle")} />
      <PageBody>
        <div className="btn-group mb-3 flex-wrap">
          {TABS.map((tabItem) => (
            <button key={tabItem.key} type="button" className={`btn btn-sm ${tab === tabItem.key ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setTab(tabItem.key)}>
              {t(tabItem.labelKey)}
            </button>
          ))}
        </div>
        {tab === "users" && <UsersTab />}
        {tab === "roles" && <RolesTab />}
        {tab === "facilities" && <FacilitiesTab />}
        {tab === "organizations" && <OrganizationsTab />}
        {tab === "identity" && <IdentityProvidersTab />}
        {tab === "audit" && <AuditLogTab />}
        {tab === "notifications" && <NotificationTemplatesTab />}
        {tab === "webhooks" && <WebhooksTab />}
        {tab === "skills" && <SkillsTab />}
        {tab === "tenant" && <TenantTab />}
      </PageBody>
    </>
  );
}
