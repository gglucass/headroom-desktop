import { describe, expect, it } from "vitest";
import { mockDashboard, researchCandidates } from "./mockData";

describe("mock dashboard seed", () => {
  it("includes required headroom tool metadata", () => {
    const headroom = mockDashboard.tools.find((tool) => tool.id === "headroom");

    expect(headroom).toBeDefined();
    expect(headroom?.required).toBe(true);
    expect(headroom?.runtime).toBe("python");
  });

  it("keeps research matrix decisions constrained", () => {
    const decisions = new Set(researchCandidates.map((candidate) => candidate.decision));
    expect(decisions).toEqual(new Set(["include", "defer", "research"]));
  });
});
