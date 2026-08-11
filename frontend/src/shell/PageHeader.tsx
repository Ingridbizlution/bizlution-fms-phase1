import type { ReactNode } from "react";

interface PageHeaderProps {
  pretitle?: string;
  title: string;
  actions?: ReactNode;
}

export function PageHeader({ pretitle, title, actions }: PageHeaderProps) {
  return (
    <div className="page-header d-print-none">
      <div className="container-xl">
        <div className="row g-2 align-items-center">
          <div className="col">
            {pretitle && <div className="page-pretitle">{pretitle}</div>}
            <h2 className="page-title">{title}</h2>
          </div>
          {actions && <div className="col-auto ms-auto d-print-none">{actions}</div>}
        </div>
      </div>
    </div>
  );
}
