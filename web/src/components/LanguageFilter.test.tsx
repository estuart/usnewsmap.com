import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { choiceLabel, LanguageFilter, languageChoices, languageSummary } from "./LanguageFilter";
import type { MetaLanguage } from "../api/types";
import { PlaceTable } from "./PlaceTable";

const META: MetaLanguage[] = [
  { code: "eng", name: "English", titles: 4, pages: 1248 },
  { code: "ger", name: "German", titles: 2, pages: 624 },
  { code: "spa", name: "Spanish", titles: 1, pages: 312 },
  { code: "nav", name: "Navajo", titles: 1, pages: null },
];

describe("language choices", () => {
  it("keep the API's order, take names from languages.ts, and keep unknown chosen codes", () => {
    const c = languageChoices(META, ["xyz", "ger"]);
    expect(c.map((x) => x.code)).toEqual(["eng", "ger", "spa", "nav", "xyz"]);
    // languages.ts has no Navajo: the API's name is used.
    expect(c.map((x) => x.name)).toEqual(["English", "German", "Spanish", "Navajo", "Xyz"]);
    expect(c.map((x) => choiceLabel(x))).toEqual([
      "English (4 newspapers)",
      "German (2 newspapers)",
      "Spanish (1 newspaper)",
      "Navajo (1 newspaper)",
      "Xyz (no newspapers)",
    ]);
    expect(languageChoices(undefined, [])).toEqual([]);
  });

  it("summarise the choice on the button", () => {
    const c = languageChoices(META, []);
    expect(languageSummary([], c)).toBe("Languages: any");
    expect(languageSummary(["ger"], c)).toBe("Languages: German");
    expect(languageSummary(["ger", "spa"], c)).toBe("Languages: German, Spanish");
    expect(languageSummary(["eng", "ger", "spa"], c)).toBe("Languages: 3 chosen");
  });

  it("speak of pages on a version with page languages (#289)", () => {
    const c = languageChoices(META, ["xyz"]);
    expect(languageSummary([], c, true)).toBe("Languages: any");
    expect(languageSummary(["eng"], c, true)).toBe("Languages: pages in English");
    expect(languageSummary(["ger", "spa"], c, true)).toBe("Languages: pages in German, Spanish");
    expect(languageSummary(["eng", "ger", "spa"], c, true)).toBe("Languages: pages in 3 languages");
    expect(c.map((x) => choiceLabel(x, true))).toEqual([
      "English (1,248 pages)",
      "German (624 pages)",
      "Spanish (312 pages)",
      "Navajo (no pages)",
      "Xyz (no pages)",
    ]);
  });
});

describe("LanguageFilter", () => {
  afterEach(cleanup);

  it("opens a labelled checklist, reports sorted choices and closes on Escape", () => {
    const onChange = vi.fn();
    render(<LanguageFilter id="t" choices={languageChoices(META, [])} value={["spa"]} onChange={onChange} />);
    const button = screen.getByRole("button", { name: "Languages: Spanish" });
    expect(button.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(button);
    expect(button.getAttribute("aria-expanded")).toBe("true");
    const group = screen.getByRole("group", { name: "Newspaper languages" });
    expect(group).toBeTruthy();
    const spanish = screen.getByRole("checkbox", { name: "Spanish (1 newspaper)" }) as HTMLInputElement;
    expect(spanish.checked).toBe(true);
    fireEvent.click(screen.getByRole("checkbox", { name: "German (2 newspapers)" }));
    expect(onChange).toHaveBeenLastCalledWith(["ger", "spa"]);
    fireEvent.click(spanish);
    expect(onChange).toHaveBeenLastCalledWith([]);
    fireEvent.click(screen.getByRole("button", { name: "Clear languages" }));
    expect(onChange).toHaveBeenLastCalledWith([]);
    fireEvent.keyDown(spanish, { key: "Escape" });
    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(button);
  });

  it("names page languages and counts pages when the version has them (#289)", () => {
    render(<LanguageFilter id="t" choices={languageChoices(META, [])} value={["ger"]} onChange={vi.fn()} byPage />);
    fireEvent.click(screen.getByRole("button", { name: "Languages: pages in German" }));
    expect(screen.getByRole("group", { name: "Page languages" })).toBeTruthy();
    expect(screen.getByText(/Pages written in any of these, found from each page's words\./)).toBeTruthy();
    const german = screen.getByRole("checkbox", { name: "German (624 pages)" }) as HTMLInputElement;
    expect(german.checked).toBe(true);
  });
});

describe("PlaceTable under a language filter", () => {
  afterEach(cleanup);

  it("leaves out the share of pages published when the search has no baselines (an older version under a language filter)", () => {
    const rows = [
      {
        id: "P1",
        name: "Place 1",
        state: "NE",
        precision: "city",
        position: [0, 0] as [number, number],
        value: 3,
        rel: Number.NaN,
        firstDay: -1,
        lastDay: -1,
      },
    ];
    render(<PlaceTable rows={rows} onSelect={() => undefined} selected="" share={false} />);
    expect(screen.getAllByRole("columnheader").map((h) => h.textContent)).toEqual([
      "Place",
      "State",
      "Pages",
      "First appearance",
      "Last seen",
      "Median date",
      "Middle half",
    ]);
    cleanup();
    render(<PlaceTable rows={rows} onSelect={() => undefined} selected="" />);
    expect(screen.getAllByRole("columnheader").map((h) => h.textContent)).toContain("Share of pages published");
  });
});
