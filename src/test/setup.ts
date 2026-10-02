import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

// Node 25+ defines its own localStorage/sessionStorage globals (localStorage
// is undefined without --localstorage-file), and vitest's jsdom environment
// skips window keys the Node global already has, so jsdom's Storage never
// reached the tests. Point both back at jsdom's.
const { window: domWindow } = (globalThis as unknown as { jsdom: { window: Window } }).jsdom;
for (const key of ["localStorage", "sessionStorage"] as const) {
  Object.defineProperty(globalThis, key, {
    value: domWindow[key],
    configurable: true,
    writable: true,
  });
}

afterEach(() => {
  cleanup();
});
