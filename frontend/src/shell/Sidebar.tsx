import { useRef } from "react";
import { NavLink } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { Collapse } from "@tabler/core/dist/js/tabler.esm.min.js";
import { Can } from "../auth/Can";
import { NAV_ITEMS } from "./nav";

export function Sidebar() {
  const { t } = useTranslation();
  const menuRef = useRef<HTMLDivElement>(null);
  // Plain Bootstrap `.collapse` doesn't close itself on a nav-link click — React Router
  // swaps the route but never tells the Collapse instance to hide, so the expanded mobile
  // menu stays open (pushing page content down) until the user re-taps the hamburger.
  // Above `lg` the collapse is always shown via CSS regardless of this, so calling
  // `.hide()` on desktop is a harmless no-op.
  const closeMenu = () => {
    const el = menuRef.current;
    if (el) Collapse.getOrCreateInstance(el, { toggle: false }).hide();
  };
  return (
    <aside className="navbar navbar-vertical navbar-expand-lg" data-bs-theme="dark">
      <div className="container-fluid">
        <button
          className="navbar-toggler"
          type="button"
          data-bs-toggle="collapse"
          data-bs-target="#sidebar-menu"
          aria-controls="sidebar-menu"
          aria-expanded="false"
          aria-label="Toggle navigation"
        >
          <span className="navbar-toggler-icon" />
        </button>
        <NavLink to="/" className="navbar-brand navbar-brand-autodark">
          <strong>FMS</strong>
        </NavLink>
        <div className="collapse navbar-collapse" id="sidebar-menu" ref={menuRef}>
          <ul className="navbar-nav pt-lg-3">
            {NAV_ITEMS.map((item) => {
              const link = (
                <li className="nav-item" key={item.to}>
                  <NavLink to={item.to} end={item.to === "/"} className="nav-link" onClick={closeMenu}>
                    <span className="nav-link-icon d-md-none d-lg-inline-block">
                      <item.icon size={20} stroke={1.75} style={{ color: item.accentColor }} />
                    </span>
                    <span className="nav-link-title">{t(item.labelKey)}</span>
                  </NavLink>
                </li>
              );
              if (!item.permission) return link;
              return (
                <Can permission={item.permission} scope={item.scope} key={item.to}>
                  {link}
                </Can>
              );
            })}
          </ul>
        </div>
      </div>
    </aside>
  );
}
