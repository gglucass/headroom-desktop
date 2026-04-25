import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { LauncherShell } from "./LauncherShell";

function renderShell(overrides: Partial<React.ComponentProps<typeof LauncherShell>> = {}) {
  const props: React.ComponentProps<typeof LauncherShell> = {
    shellClassName: "intro-shell",
    spinnerClassName: "intro-shell__spinner",
    copyClassName: "intro-shell__copy",
    onMouseDown: () => {},
    version: "0.3.0",
    children: <p data-testid="launcher-child">Stage content</p>,
    ...overrides
  };
  return render(<LauncherShell {...props} />);
}

describe("LauncherShell", () => {
  it("renders the version badge and any child stage content", () => {
    renderShell();

    expect(screen.getByText("v0.3.0")).toBeInTheDocument();
    expect(screen.getByTestId("launcher-child")).toHaveTextContent(
      "Stage content"
    );
  });

  it("renders the spinner image by default", () => {
    const { container } = renderShell();
    // Two img tags: the badge logo and the spinner. With showSpinner=true,
    // both should be present.
    const imgs = container.querySelectorAll("img");
    expect(imgs.length).toBe(2);
  });

  it("hides the spinner when showSpinner is false", () => {
    const { container } = renderShell({ showSpinner: false });
    const imgs = container.querySelectorAll("img");
    expect(imgs.length).toBe(1);
  });

  it("forwards the mouseDown event so window-drag handlers wire through", async () => {
    const onMouseDown = vi.fn();
    renderShell({ onMouseDown });
    const user = userEvent.setup();

    // userEvent.pointer with mousedown lets us simulate the drag-start.
    await user.pointer([
      {
        target: screen.getByText("Stage content").closest("section")!,
        keys: "[MouseLeft>]"
      }
    ]);

    expect(onMouseDown).toHaveBeenCalled();
  });
});
