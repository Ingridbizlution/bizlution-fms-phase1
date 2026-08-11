import type { ReactNode } from "react";
import { Sidebar } from "./Sidebar";
import { Navbar } from "./Navbar";

export function AppShell({ children }: { children: ReactNode }) {
  return (
    <div className="page">
      <Sidebar />
      <div className="page-wrapper">
        <Navbar />
        {children}
      </div>
    </div>
  );
}
