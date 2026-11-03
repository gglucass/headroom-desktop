import { describe, expect, it } from "vitest";
import plugin from "./headroom-plugin.js";

type Model = {
  providerID: string;
  id: string;
  package: string;
  settings: { baseURL: string };
  headers?: Record<string, string>;
};

// Runs the plugin's OpenCode 2.x setup against a fake model registry.
async function routedModels(models: Model[]) {
  const ctx = {
    options: { proxyUrl: "http://127.0.0.1:6767" },
    location: { project: { id: "proj" }, directory: "/tmp/proj" },
    model: {
      transform: async (fn: (registry: unknown) => void) =>
        fn({
          list: () => models,
          provider: { get: () => undefined },
          update: (providerID: string, modelID: string, edit: (draft: Model) => void) =>
            edit(models.find((m) => m.providerID === providerID && m.id === modelID)!),
        }),
    },
    tool: { transform: async () => {} },
    shell: { hook: async () => {} },
  };
  const dispose = await plugin.setup(ctx);
  dispose();
  return models;
}

describe("OpenCode 2.x model routing", () => {
  it("routes OpenRouter chat models through the proxy", async () => {
    const [model] = await routedModels([
      {
        providerID: "openrouter",
        id: "anthropic/claude-sonnet-4.5",
        package: "@opencode/ai/providers/openrouter",
        settings: { baseURL: "https://openrouter.ai/api/v1" },
      },
    ]);
    expect(model.settings.baseURL).toBe("http://127.0.0.1:6767/v1");
    expect(model.headers).toMatchObject({
      "x-headroom-base-url": "https://openrouter.ai",
      "x-headroom-original-path": "/api/v1/chat/completions",
    });
  });

  it("leaves a provider the transform does not handle alone", async () => {
    const [model] = await routedModels([
      {
        providerID: "anthropic",
        id: "claude-sonnet-4-5",
        package: "@opencode/ai/providers/anthropic",
        settings: { baseURL: "https://api.anthropic.com/v1" },
      },
    ]);
    expect(model.settings.baseURL).toBe("https://api.anthropic.com/v1");
    expect(model.headers).toBeUndefined();
  });
});
