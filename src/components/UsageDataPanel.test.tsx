import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { UsageDataPanel } from "./UsageDataPanel";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));

describe("UsageDataPanel", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("toggles usage data off and back on through the backend", async () => {
    let enabled = true;
    invokeMock.mockImplementation((command: string, args?: { enabled: boolean }) => {
      if (command === "get_usage_data_enabled") return Promise.resolve(enabled);
      if (command === "set_usage_data_enabled") {
        enabled = args!.enabled;
        return Promise.resolve(enabled);
      }
      throw new Error(`unexpected command ${command}`);
    });
    render(<UsageDataPanel />);

    const toggle = screen.getByRole("switch");
    await waitFor(() => expect(toggle).not.toBeDisabled());
    expect(toggle).toHaveAttribute("aria-checked", "true");

    await userEvent.click(toggle);
    await waitFor(() => expect(toggle).toHaveAttribute("aria-checked", "false"));
    expect(invokeMock).toHaveBeenCalledWith("set_usage_data_enabled", { enabled: false });

    await userEvent.click(toggle);
    await waitFor(() => expect(toggle).toHaveAttribute("aria-checked", "true"));
    expect(invokeMock).toHaveBeenLastCalledWith("set_usage_data_enabled", { enabled: true });
  });

  it("keeps the last known state when saving fails", async () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    invokeMock.mockImplementation((command: string) =>
      command === "get_usage_data_enabled"
        ? Promise.resolve(true)
        : Promise.reject(new Error("client-setup.json unwritable"))
    );
    render(<UsageDataPanel />);

    const toggle = screen.getByRole("switch");
    await waitFor(() => expect(toggle).not.toBeDisabled());
    await userEvent.click(toggle);

    await waitFor(() => expect(toggle).not.toBeDisabled());
    expect(toggle).toHaveAttribute("aria-checked", "true");
    expect(consoleError).toHaveBeenCalled();
    consoleError.mockRestore();
  });
});
