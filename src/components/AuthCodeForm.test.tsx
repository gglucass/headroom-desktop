import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { AuthCodeForm } from "./AuthCodeForm";

function renderForm(overrides: Partial<React.ComponentProps<typeof AuthCodeForm>> = {}) {
  const props: React.ComponentProps<typeof AuthCodeForm> = {
    lead: "Sign in to continue",
    email: "",
    onEmailChange: vi.fn(),
    emailValid: false,
    code: "",
    onCodeChange: vi.fn(),
    codeRequested: false,
    requestBusy: false,
    verifyBusy: false,
    error: null,
    success: null,
    onRequestCode: vi.fn(),
    onVerify: vi.fn(),
    ...overrides
  };

  render(<AuthCodeForm {...props} />);
  return props;
}

describe("AuthCodeForm", () => {
  it("renders the email step and only enables sending for a valid idle email", async () => {
    const props = renderForm({ email: "person@example.com", emailValid: true });

    expect(screen.getByText("Sign in to continue")).toBeInTheDocument();
    await userEvent.type(screen.getByPlaceholderText("you@example.com"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Send code" }));

    expect(props.onEmailChange).toHaveBeenLastCalledWith("person@example.comx");
    expect(props.onRequestCode).toHaveBeenCalledTimes(1);
    expect(screen.queryByPlaceholderText("6-digit code")).not.toBeInTheDocument();
  });

  it("blocks code requests when the email is invalid", () => {
    renderForm();
    expect(screen.getByRole("button", { name: "Send code" })).toBeDisabled();
  });

  it("shows the sending state", () => {
    renderForm({ email: "person@example.com", emailValid: true, requestBusy: true });
    expect(screen.getByRole("button", { name: /Sending/ })).toBeDisabled();
  });

  it("renders the code step and verifies a non-blank code", async () => {
    const props = renderForm({
      codeRequested: true,
      email: "person@example.com",
      emailValid: true,
      code: "123456"
    });

    expect(screen.getByRole("button", { name: "Resend code" })).toBeEnabled();
    await userEvent.type(screen.getByPlaceholderText("6-digit code"), "7");
    await userEvent.click(screen.getByRole("button", { name: "Verify" }));

    expect(props.onCodeChange).toHaveBeenLastCalledWith("1234567");
    expect(props.onVerify).toHaveBeenCalledTimes(1);
  });

  it("blocks verification while the code is blank", () => {
    renderForm({ codeRequested: true, code: "   " });
    expect(screen.getByRole("button", { name: "Verify" })).toBeDisabled();
  });

  it("shows the verifying state", () => {
    renderForm({ codeRequested: true, code: "123456", verifyBusy: true });
    expect(screen.getByRole("button", { name: /Verifying/ })).toBeDisabled();
  });

  it("shows error before success", () => {
    renderForm({ error: "That code expired", success: "Check your email" });

    expect(screen.getByText("That code expired")).toBeInTheDocument();
    expect(screen.queryByText("Check your email")).not.toBeInTheDocument();
  });

  it("shows success when there is no error", () => {
    renderForm({ success: "Check your email" });

    expect(screen.getByText("Check your email")).toBeInTheDocument();
  });
});
