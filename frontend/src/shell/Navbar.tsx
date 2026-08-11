import { IconBell, IconChevronDown, IconLogout, IconLanguage } from "@tabler/icons-react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { Tooltip } from "@tabler/core/dist/js/tabler.esm.min.js";
import { listNotifications, markNotificationRead } from "../api/notifications";
import { useAuth } from "../auth/AuthContext";
import { setLanguage, SUPPORTED_LANGUAGES, type SupportedLanguage } from "../i18n";

/** Bootstrap's Tooltip needs manual init (Tabler's JS bundle doesn't auto-scan on mount for
 *  content React adds after initial load); re-created on each language change so the title
 *  text stays translated. Not using `data-bs-toggle="tooltip"` here since these links already
 *  use `data-bs-toggle="dropdown"` — one attribute can't hold both. */
function useIconTooltip(title: string) {
  const ref = useRef<HTMLAnchorElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const tooltip = new Tooltip(el, { title, placement: "bottom" });
    return () => tooltip.dispose();
  }, [title]);
  return ref;
}

const LANGUAGE_LABELS: Record<SupportedLanguage, string> = {
  en: "English",
  "zh-TW": "繁體中文",
  ja: "日本語",
};

export function Navbar() {
  const { t, i18n } = useTranslation();
  const { currentUser, facilityId, setFacilityId, logout } = useAuth();
  const queryClient = useQueryClient();
  const facilities = currentUser?.accessible_facilities ?? [];
  const activeFacility = facilities.find((f) => f.id === facilityId);
  const displayName = currentUser?.user?.display_name ?? currentUser?.user?.username ?? "";
  const roleLabel = currentUser?.roles?.[0]?.role_code ?? "";

  const notificationsQuery = useQuery({
    queryKey: ["notifications"],
    queryFn: () => listNotifications(),
    refetchInterval: 60_000,
  });
  const unreadCount = notificationsQuery.data?.meta.unread_count ?? 0;

  const markRead = useMutation({
    mutationFn: (id: string) => markNotificationRead(id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["notifications"] }),
  });

  const bellRef = useIconTooltip(t("common.notifications"));
  const languageRef = useIconTooltip(t("common.selectLanguage"));

  return (
    <header className="navbar navbar-expand-md navbar-light d-print-none">
      <div className="container-xl">
        <div className="navbar-nav flex-row order-md-last">
          {facilities.length > 1 && (
            <div className="nav-item dropdown me-2">
              <a href="#" className="nav-link d-flex lh-1 text-reset p-0" data-bs-toggle="dropdown" aria-label={t("common.selectFacility")}>
                {/* Always visible, not just at xl+ — on mobile this is a user's only way to see
                    which facility they're looking at without opening the dropdown (most pages'
                    own headers don't restate it). Truncated so a long facility name can't push
                    the bell/language/avatar icons off the navbar on narrow screens. */}
                <span className="text-truncate mx-2" style={{ maxWidth: 140 }}>
                  {activeFacility?.name ?? t("common.selectFacility")}
                </span>
                <IconChevronDown size={16} />
              </a>
              <div className="dropdown-menu dropdown-menu-end dropdown-menu-arrow">
                {facilities.map((f) => (
                  <button
                    key={f.id}
                    type="button"
                    className={`dropdown-item ${f.id === facilityId ? "active" : ""}`}
                    onClick={() => f.id && setFacilityId(f.id)}
                  >
                    {f.name}
                  </button>
                ))}
              </div>
            </div>
          )}

          <div className="nav-item dropdown me-2">
            <a ref={bellRef} href="#" className="nav-link px-0 position-relative" data-bs-toggle="dropdown" aria-label={t("common.notifications")}>
              <IconBell size={20} />
              {unreadCount > 0 && <span className="badge bg-red badge-notification badge-pill">{unreadCount}</span>}
            </a>
            <div className="dropdown-menu dropdown-menu-end dropdown-menu-arrow dropdown-menu-card">
              <div className="card">
                <div className="card-header">
                  <h3 className="card-title">{t("common.notifications")}</h3>
                </div>
                <div className="list-group list-group-flush" style={{ maxHeight: 320, overflowY: "auto" }}>
                  {notificationsQuery.data?.data.length ? (
                    notificationsQuery.data.data.map((n) => (
                      <button
                        key={n.id}
                        type="button"
                        className={`list-group-item list-group-item-action text-start ${n.read_at ? "" : "fw-medium"}`}
                        onClick={() => !n.read_at && markRead.mutate(n.id)}
                      >
                        <div>{n.subject ?? n.body}</div>
                        <div className="text-secondary small">{new Date(n.created_at).toLocaleString()}</div>
                      </button>
                    ))
                  ) : (
                    <div className="card-body text-secondary">{t("common.allCaughtUp")}</div>
                  )}
                </div>
              </div>
            </div>
          </div>

          <div className="nav-item dropdown me-2">
            <a ref={languageRef} href="#" className="nav-link d-flex lh-1 text-reset p-0" data-bs-toggle="dropdown" aria-label={t("common.selectLanguage")}>
              <IconLanguage size={20} />
              <span className="d-none d-xl-inline mx-2">{LANGUAGE_LABELS[i18n.language as SupportedLanguage] ?? LANGUAGE_LABELS.en}</span>
            </a>
            <div className="dropdown-menu dropdown-menu-end dropdown-menu-arrow">
              {SUPPORTED_LANGUAGES.map((lang) => (
                <button
                  key={lang}
                  type="button"
                  className={`dropdown-item ${i18n.language === lang ? "active" : ""}`}
                  onClick={() => setLanguage(lang)}
                >
                  {LANGUAGE_LABELS[lang]}
                </button>
              ))}
            </div>
          </div>

          <div className="nav-item dropdown">
            <a href="#" className="nav-link d-flex lh-1 text-reset p-0" data-bs-toggle="dropdown" aria-label="Open user menu">
              <span className="avatar avatar-sm">{displayName.slice(0, 2).toUpperCase()}</span>
              <div className="d-none d-xl-block ps-2">
                <div>{displayName}</div>
                <div className="mt-1 small text-secondary">{roleLabel}</div>
              </div>
            </a>
            <div className="dropdown-menu dropdown-menu-end dropdown-menu-arrow">
              <button type="button" className="dropdown-item" onClick={() => void logout()}>
                <IconLogout size={16} className="me-2" />
                {t("common.logOut")}
              </button>
            </div>
          </div>
        </div>
      </div>
    </header>
  );
}
