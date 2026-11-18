import { afterEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

describe("analytics helpers", () => {
  afterEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    vi.resetModules();
  });

  it("sends Sentry envelopes only while usage data is on", async () => {
    const send = vi.fn().mockResolvedValue({ statusCode: 200 });
    const { gateOnUsageData } = await import("./analytics");
    const transport = gateOnUsageData({ send, flush: vi.fn() });

    invokeMock.mockResolvedValueOnce(false);
    await expect(transport.send("event")).resolves.toEqual({});
    expect(send).not.toHaveBeenCalled();
    expect(invokeMock).toHaveBeenCalledWith("get_usage_data_enabled");

    invokeMock.mockResolvedValueOnce(true);
    await expect(transport.send("session")).resolves.toEqual({ statusCode: 200 });
    expect(send).toHaveBeenCalledWith("session");
  });
});
