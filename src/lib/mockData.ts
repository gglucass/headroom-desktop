import type { DashboardState } from "./types";
import packageJson from "../../package.json";

export const mockDashboard: DashboardState = {
  appVersion: packageJson.version,
  launchExperience: "first_run",
  bootstrapComplete: false,
  pythonRuntimeInstalled: false,
  lifetimeRequests: 0,
  firstPromptRequestSeen: false,
  lifetimeEstimatedSavingsUsd: 0,
  lifetimeEstimatedTokensSaved: 0,
  sessionRequests: 0,
  sessionEstimatedSavingsUsd: 0,
  sessionEstimatedTokensSaved: 0,
  sessionSavingsPct: 0,
  outputReduction: null,
  learnerProgress: null,
  savingsBreakdown: null,
  dailySavings: [],
  hourlySavings: [],
  savingsHistoryLoaded: false,
  tools: [
    {
      id: "headroom",
      name: "Headroom",
      description: "Mandatory prompt compaction stage for coding-focused calls.",
      runtime: "python",
      required: true,
      enabled: true,
      status: "not_installed",
      sourceUrl: "https://pypi.org/project/headroom-ai/",
      version: "pending"
    }
  ],
  clients: [
    {
      id: "claude_code",
      name: "Claude Code",
      installed: true,
      configured: false,
      health: "attention",
      notes: ["Detected on this machine", "Needs proxy configuration"]
    }
  ],
  recentUsage: [],
  // Mock represents an already-accepted user so the terms gate never flashes
  // over the initial mock state before the real dashboard loads.
  requiredTermsVersion: 1,
  acceptedTermsVersion: 1,
  termsUrl: "https://extraheadroom.com/terms",
  tipUnseen: false
};
