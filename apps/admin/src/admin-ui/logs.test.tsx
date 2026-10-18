import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { I18nProvider } from "@lingui/react";
import { describe, expect, it } from "vite-plus/test";

import { i18n } from "../i18n";
import { RequestLogIdentity } from "./logs";

const apiKeys = [{ id: "key-1", name: "Daily Codex" }];
const accounts = [
  { id: "account-a", name: "Primary account" },
  { id: "account-b", name: "Backup account" },
];

function renderIdentity(providerAccountId?: string) {
  return renderToStaticMarkup(
    createElement(
      I18nProvider,
      { i18n },
      createElement(
        MemoryRouter,
        null,
        createElement(RequestLogIdentity, {
          log: { apiKeyId: "key-1", providerAccountId },
          apiKeys,
          accounts,
        }),
      ),
    ),
  );
}

describe("Request log identity", () => {
  it("links the client key and the account that handled this attempt", () => {
    const markup = renderIdentity("account-b");
    expect(markup).toContain("Daily Codex");
    expect(markup).toContain('href="/keys/key-1"');
    expect(markup).toContain("Backup account");
    expect(markup).toContain('href="/accounts/account-b"');
    expect(markup).not.toContain("Primary account");
  });

  it("keeps retry attempts associated with their own accounts", () => {
    expect(renderIdentity("account-a")).toContain("Primary account");
    expect(renderIdentity("account-b")).toContain("Backup account");
  });

  it("shows an unassigned backend without inventing an account link", () => {
    const markup = renderIdentity();
    expect(markup).toContain("No backend assigned");
    expect(markup).not.toContain('href="/accounts/');
    expect(markup).toContain('href="/keys/key-1"');
  });
});
