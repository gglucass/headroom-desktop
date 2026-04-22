import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  test: {
    coverage: {
      provider: "v8",
      include: ["src/components/**/*.tsx", "src/lib/**/*.ts"],
      exclude: [
        "src/lib/types.ts",
        "src/**/*.test.{ts,tsx}",
        // Integration-heavy: Tauri `invoke` inside useEffect, untestable under
        // the project's SSR-only setup. Revisit if jsdom/testing-library lands.
        "src/components/OptimizePanel.tsx"
      ],
      reporter: ["text", "json-summary", "html"],
      thresholds: {
        lines: 90,
        statements: 90,
        functions: 90,
        branches: 85
      }
    }
  },
  server: {
    port: 1420,
    strictPort: true
  }
});
