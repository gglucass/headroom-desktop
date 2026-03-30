import type { DashboardState, ResearchCandidate } from "./types";

export const mockDashboard: DashboardState = {
  appVersion: "0.1.0",
  launchExperience: "first_run",
  bootstrapComplete: false,
  pythonRuntimeInstalled: false,
  lifetimeRequests: 0,
  lifetimeEstimatedSavingsUsd: 0,
  lifetimeEstimatedTokensSaved: 0,
  sessionRequests: 0,
  sessionEstimatedSavingsUsd: 0,
  sessionEstimatedTokensSaved: 0,
  sessionSavingsPct: 0,
  dailySavings: [],
  hourlySavings: [],
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
  insights: [
    {
      id: "insight-1",
      category: "savings",
      severity: "info",
      title: "Recent sessions are compressing well",
      recommendation: "Finish bootstrap to keep Headroom active on every detected workspace.",
      evidence: "Recent sampled requests show 14-22% prompt reduction."
    },
    {
      id: "insight-2",
      category: "health",
      severity: "warning",
      title: "Runtime is not fully set up yet",
      recommendation: "Install the managed Python runtime to keep Headroom active.",
      evidence: "Bootstrap has not completed yet on this machine."
    }
  ]
};

export const researchCandidates: ResearchCandidate[] = [
  {
    name: "Headroom",
    category: "Prompt optimization",
    repository: "https://github.com/chopratejas/headroom",
    runtime: "Python",
    license: "Research required",
    localOnlyFit: "Strong fit as localhost proxy/gateway",
    installMethod: "Managed Python environment + pinned package install",
    maintenance: "Core v1 dependency",
    decision: "include",
    notes: "Mandatory optimizer stage in v1."
  },
  {
    name: "Vitals",
    category: "Project health",
    repository: "https://github.com/chopratejas/vitals",
    runtime: "Python",
    license: "Research required",
    localOnlyFit: "Strong fit for local daily scans",
    installMethod: "Managed Python environment + pinned package install",
    maintenance: "Track compatibility alongside Headroom",
    decision: "include",
    notes: "Primary code analysis/scanner in v1."
  },
  {
    name: "claw-compactor",
    category: "Prompt optimization",
    repository: "https://github.com/aeromomo/claw-compactor",
    runtime: "Python",
    license: "Research required",
    localOnlyFit: "Candidate if adapter contract is stable",
    installMethod: "Managed Python environment + optional install",
    maintenance: "Medium",
    decision: "research",
    notes: "Evaluate CLI surface area and long-term maintenance."
  },
  {
    name: "rtk",
    category: "Token optimization",
    repository: "https://github.com/rtk-ai/rtk",
    runtime: "Rust binary",
    license: "Research required",
    localOnlyFit: "Strong fit for local shell output compression",
    installMethod: "Managed binary download + Claude hook setup",
    maintenance: "Track alongside Claude Code integration",
    decision: "include",
    notes: "Installed by default so Claude Code bash commands are auto-rewritten through RTK."
  },
  {
    name: "claude-cognitive",
    category: "Workflow enhancement",
    repository: "https://github.com/GMaN1911/claude-cognitive",
    runtime: "Non-v1 fit",
    license: "Research required",
    localOnlyFit: "More shell/user-profile oriented than Headroom v1 should assume",
    installMethod: "External/manual",
    maintenance: "Medium",
    decision: "defer",
    notes: "Outside the Python-only policy for v1."
  }
];
