import type { ReactNode } from "react";

export function PageBody({ children }: { children: ReactNode }) {
  return (
    <div className="page-body">
      <div className="container-xl">{children}</div>
    </div>
  );
}
