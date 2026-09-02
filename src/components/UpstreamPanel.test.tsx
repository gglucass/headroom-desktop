import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { UpstreamPanel } from "./UpstreamPanel";
import type { UpstreamOverrideView } from "../lib/types";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));

const off: UpstreamOverrideView = { mode: "off", baseUrl: "", hasToken: false };
const configured: UpstreamOverrideView = {
  mode: "override",
  baseUrl: "https://api.z.ai/api/anthropic",
  hasToken: true
};

function respond(current: UpstreamOverrideView, saved?: UpstreamOverrideView) {
  invokeMock.mockImplementation((command: string) => {
    if (command === "get_upstream_override") return Promise.resolve(current);
    if (command === "save_upstream_override") return Promise.resolve(saved ?? current);
    throw new Error(`unexpected command ${command}`);
  });
}

describe("UpstreamPanel", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("sends what the user typed, and restarts onto it", async () => {
    respond(off, { ...configured, hasToken: true });
    const user = userEvent.setup();
    render(<UpstreamPanel />);

    await screen.findByLabelText("Provider base URL");
    await user.type(screen.getByLabelText("Provider base URL"), "https://api.z.ai/api/anthropic");
    await user.type(screen.getByLabelText("Provider auth token"), "secret-token");
    await user.click(screen.getByRole("button", { name: "Save and restart" }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("save_upstream_override", {
        mode: "override",
        baseUrl: "https://api.z.ai/api/anthropic",
        token: "secret-token"
      });
    });
    expect(await screen.findByText(/restarted on this provider/)).toBeInTheDocument();
  });

  /// The token field starts blank even when one is stored, so an untouched
  /// save must not be read as "clear it".
  it("leaves a stored token alone when the field is untouched", async () => {
    respond(configured);
    const user = userEvent.setup();
    render(<UpstreamPanel />);

    await waitFor(() => {
      expect(screen.getByLabelText("Provider auth token")).toHaveAttribute(
        "placeholder",
        "Stored — type to replace"
      );
    });
    await user.click(screen.getByRole("button", { name: "Save and restart" }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("save_upstream_override", {
        mode: "override",
        baseUrl: "https://api.z.ai/api/anthropic",
        token: null
      });
    });
  });

  it("clears the stored token on request", async () => {
    respond(configured, { ...configured, hasToken: false });
    const user = userEvent.setup();
    render(<UpstreamPanel />);

    await screen.findByRole("button", { name: "Remove stored token" });
    await user.click(screen.getByRole("button", { name: "Remove stored token" }));
    await user.click(screen.getByRole("button", { name: "Save and restart" }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("save_upstream_override", {
        mode: "override",
        baseUrl: "https://api.z.ai/api/anthropic",
        token: ""
      });
    });
  });

  it("surfaces a rejected base URL instead of pretending it saved", async () => {
    invokeMock.mockImplementation((command: string) => {
      if (command === "get_upstream_override") return Promise.resolve(off);
      return Promise.reject("The base URL must start with http:// or https://");
    });
    const user = userEvent.setup();
    render(<UpstreamPanel />);

    await screen.findByLabelText("Provider base URL");
    await user.type(screen.getByLabelText("Provider base URL"), "api.z.ai");
    await user.click(screen.getByRole("button", { name: "Save and restart" }));

    expect(await screen.findByText(/must start with http/)).toBeInTheDocument();
    expect(screen.queryByText(/restarted/)).not.toBeInTheDocument();
  });

  /// Clearing the URL is the only way to turn the provider back off, so it has
  /// to reach the backend as "off" -- that is what drops the stored token too.
  it("turns the provider off when the URL is emptied", async () => {
    respond(configured, off);
    const user = userEvent.setup();
    render(<UpstreamPanel />);

    await screen.findByDisplayValue("https://api.z.ai/api/anthropic");
    await user.clear(screen.getByLabelText("Provider base URL"));
    await user.click(screen.getByRole("button", { name: "Save and restart" }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("save_upstream_override", {
        mode: "off",
        baseUrl: "",
        token: null
      });
    });
    expect(await screen.findByText(/restarted on Anthropic/)).toBeInTheDocument();
  });
});
