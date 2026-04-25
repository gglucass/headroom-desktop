import type { MouseEvent, ReactNode } from "react";

import headroomLogo from "../assets/headroom-logo.svg";

export interface LauncherShellProps {
  shellClassName: string;
  spinnerClassName: string;
  copyClassName: string;
  onMouseDown: (event: MouseEvent<HTMLElement>) => void;
  version: string;
  children: ReactNode;
  showSpinner?: boolean;
}

/// Presentational chrome around the launcher's onboarding screens.
/// Holds no state; the active stage's content is passed via `children`.
export function LauncherShell({
  shellClassName,
  spinnerClassName,
  copyClassName,
  onMouseDown,
  version,
  children,
  showSpinner = true,
}: LauncherShellProps) {
  return (
    <main className="app-shell app-shell--launcher">
      <section className={shellClassName} onMouseDown={onMouseDown}>
        <div className="hero__badge hero__badge--launcher">
          <img src={headroomLogo} alt="" aria-hidden="true" />
          <span>v{version}</span>
        </div>
        {showSpinner && (
          <img
            className={spinnerClassName}
            src={headroomLogo}
            alt=""
            aria-hidden="true"
          />
        )}
        <div className="intro-shell__content">
          <div className={copyClassName}>{children}</div>
        </div>
      </section>
    </main>
  );
}
