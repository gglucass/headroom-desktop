import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

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
    <App />
  </React.StrictMode>
);
