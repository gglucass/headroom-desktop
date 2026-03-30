import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { SectionCard } from "./SectionCard";

describe("SectionCard", () => {
  it("renders a title, subtitle, and children", () => {
    const markup = renderToStaticMarkup(
      <SectionCard subtitle="Subtitle copy" title="Usage">
        <div>Body content</div>
      </SectionCard>
    );

    expect(markup).toContain("<section");
    expect(markup).toContain('class="section-card"');
    expect(markup).toContain("<h2>Usage</h2>");
    expect(markup).toContain("<p>Subtitle copy</p>");
    expect(markup).toContain("Body content");
  });

  it("omits the subtitle paragraph when none is provided", () => {
    const markup = renderToStaticMarkup(
      <SectionCard title="No subtitle">
        <span>Child</span>
      </SectionCard>
    );

    expect(markup).toContain("<h2>No subtitle</h2>");
    expect(markup).not.toContain("<p>");
  });
});
