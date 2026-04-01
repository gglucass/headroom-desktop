import React from "react";
import ReactDOM from "react-dom/client";
import * as Sentry from "@sentry/react";
import App from "./App";
import "./styles.css";

Sentry.init({
  dsn: import.meta.env.VITE_SENTRY_DSN,
  integrations: [Sentry.browserTracingIntegration()],
  tracesSampleRate: 0.1,
});

function hideBootLoading() {
  const bootLoading = document.getElementById("boot-loading");
  if (!bootLoading) {
    return;
  }
  bootLoading.classList.add("boot-loading--done");
  window.setTimeout(() => {
    bootLoading.remove();
  }, 280);
}

window.addEventListener("headroom:boot-complete", () => {
  window.requestAnimationFrame(() => {
    hideBootLoading();
  });
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Sentry.ErrorBoundary fallback={<p>Something went wrong.</p>} showDialog>
      <App />
    </Sentry.ErrorBoundary>
  </React.StrictMode>
);
