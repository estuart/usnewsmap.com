import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { MeasureToggle } from "./MeasureToggle";
import { SkewLegend } from "./SkewLegend";
import { clearest, LANGUAGE_EXPLAINER, skewCsv, SkewLists, StateTable, type SkewRow } from "./SkewPanels";
import { useListLengths } from "./ListLength";
import { PlaceTable } from "./PlaceTable";
import type { SkewInfo } from "../lib/skewText";
import { dayNumber } from "../lib/time";

const info = (estimate: number, lower: number, upper: number, extra: Partial<SkewInfo> = {}): SkewInfo => ({
  estimate,
  lower,
  upper,
  observed: 10,
  expected: 5,
  pages: 100,
  dir: lower > 1 ? 1 : upper < 1 ? -1 : 0,
  languages: null,
  languageCounts: null,
  ...extra,
});
const row = (id: string, s: SkewInfo): SkewRow => ({ id, name: `Place ${id}`, state: "AL", skew: s });

// The lists as App renders them, with how many each shows kept above them.
// `narrow` moves them, as a phone rotation does, which remounts them.
function Lists({
  rows,
  onSelect = () => undefined,
  resetKey = "",
  narrow = false,
}: {
  rows: SkewRow[];
  onSelect?: (id: string) => void;
  resetKey?: string;
  narrow?: boolean;
}) {
  const [shown, setShown] = useListLengths(resetKey, ["above", "below"]);
  const lists = <SkewLists rows={rows} onSelect={onSelect} shown={shown} onShown={setShown} />;
  return narrow ? <div>{lists}</div> : <section>{lists}</section>;
}

describe("MeasureToggle", () => {
  afterEach(cleanup);

  it("offers pages, relative rate and median date, and reports the choice", () => {
    const onChange = vi.fn();
    render(<MeasureToggle norm="raw" onChange={onChange} />);
    const group = screen.getByRole("group", { name: "Measure" });
    const buttons = Array.from(group.querySelectorAll("button"));
    expect(buttons.map((b) => [b.textContent, b.getAttribute("aria-pressed")])).toEqual([
      ["Pages", "true"],
      ["Relative rate", "false"],
      ["Median date", "false"],
    ]);
    fireEvent.click(screen.getByRole("button", { name: "Relative rate" }));
    expect(onChange).toHaveBeenCalledWith("skew");
    fireEvent.click(screen.getByRole("button", { name: "Median date" }));
    expect(onChange).toHaveBeenCalledWith("when");
  });

  it("disables a measure the search can't show, and says why", () => {
    const onChange = vi.fn();
    render(<MeasureToggle norm="raw" onChange={onChange} unavailable={{ skew: "Not for one newspaper." }} />);
    const button = screen.getByRole("button", { name: "Relative rate" });
    expect(button.hasAttribute("disabled")).toBe(true);
    expect(button.getAttribute("title")).toBe("Not for one newspaper.");
    fireEvent.click(button);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("has no share of pages button", () => {
    render(<MeasureToggle norm="skew" onChange={() => undefined} />);
    expect(screen.queryByRole("button", { name: "Share of pages" })).toBeNull();
    expect(screen.getAllByRole("button", { pressed: true }).map((b) => b.textContent)).toEqual(["Relative rate"]);
  });

  it("explains the measure in a disclosure that Escape closes", () => {
    render(<MeasureToggle norm="skew" onChange={() => undefined} />);
    const info = screen.getByRole("button", { name: "About the relative rate" });
    expect(info.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(info);
    expect(info.getAttribute("aria-expanded")).toBe("true");
    const body = document.getElementById(info.getAttribute("aria-controls")!)!;
    expect(body.hidden).toBe(false);
    expect(body.textContent).toContain("pulled toward the typical rate");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(info.getAttribute("aria-expanded")).toBe("false");
  });
});

describe("SkewLegend", () => {
  afterEach(cleanup);

  it("says what was compared and over what, without the index version", () => {
    const { container } = render(<SkewLegend places={398} states={42} unit="month" />);
    const text = container.textContent ?? "";
    expect(text).toContain(
      "compared with the other 397 places with pages in this window, in 42 states, over the same months",
    );
    expect(text).toContain("1× is the same rate");
    expect(text).not.toContain("Index");
    expect(text).not.toMatch(/[—]/);
  });

  it("says when there is nothing to compare with", () => {
    const { container } = render(<SkewLegend places={1} states={1} unit="year" />);
    expect(container.textContent).toContain("No other place has pages in this window");
    cleanup();
    const two = render(<SkewLegend places={2} states={1} unit="year" />);
    expect(two.container.textContent).toContain(
      "the other 1 place with pages in this window, in 1 state, over the same years",
    );
  });
});

describe("lists, tables and export", () => {
  afterEach(cleanup);
  const rows = [
    row("a", info(3, 2.5, 3.5)),
    row("b", info(6, 1.2, 12)),
    row("c", info(0.3, 0.2, 0.45)),
    row("d", info(0.1, 0.05, 0.2, { languages: "Papers in German" })),
    row("e", info(1.1, 0.6, 1.8)),
  ];

  it("lists only clear differences, by the bound nearest 1×, naming papers in other languages", () => {
    const { above, below } = clearest(rows);
    expect(above.map((r) => r.id)).toEqual(["a", "b"]);
    expect(below.map((r) => r.id)).toEqual(["d", "c"]);
    const onSelect = vi.fn();
    render(<Lists rows={rows} onSelect={onSelect} />);
    fireEvent.click(screen.getByRole("button", { name: "Place a, AL" }));
    expect(onSelect).toHaveBeenCalledWith("a");
    expect(screen.getByRole("complementary", { name: "Places that differ most clearly" })).toBeTruthy();
    expect(screen.getByText("Papers in German")).toBeTruthy();
  });

  it("explains a language label behind an info button", () => {
    render(<Lists rows={rows} />);
    const info = screen.getByRole("button", { name: "What this means" });
    const tip = screen.getByRole("note", { hidden: true });
    expect(info.getAttribute("aria-expanded")).toBe("false");
    expect(tip.hidden).toBe(true);
    fireEvent.click(info);
    expect(info.getAttribute("aria-expanded")).toBe("true");
    expect(tip.hidden).toBe(false);
    expect(tip.textContent).toBe(LANGUAGE_EXPLAINER);
    fireEvent.keyDown(info, { key: "Escape" });
    expect(tip.hidden).toBe(true);
  });

  it("says how many of a place's papers are in each language", () => {
    const counts = "Of its 4 papers, 3 are in German (75%) and 1 in English (25%).";
    const skew = info(0.1, 0.05, 0.2, { languages: "Papers in German and English", languageCounts: counts });
    render(<Lists rows={[row("d", skew)]} />);
    fireEvent.click(screen.getByRole("button", { name: "What this means" }));
    expect(screen.getByRole("note").textContent).toBe(counts + LANGUAGE_EXPLAINER);
  });

  it("returns every clear difference, in order, for the lists to cut", () => {
    // Lower bounds 1.15, 1.25, ... 2.25: among them, a (2.5) comes first and b (1.2) second to last.
    const many = Array.from({ length: 12 }, (_, i) => row(`u${i}`, info(3, 1.15 + i / 10, 4)));
    const { above, below } = clearest([...many, ...rows]);
    expect(above.map((r) => r.id)).toEqual([
      "a",
      ...[11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1].map((i) => `u${i}`),
      "b",
      "u0",
    ]);
    expect(below.map((r) => r.id)).toEqual(["d", "c"]);
  });

  it("tables places and states with expected counts and ranges", () => {
    const tableRows = rows.map((r) => ({
      ...r,
      precision: "city",
      position: [0, 0] as [number, number],
      value: 10,
      rel: 0.1,
      firstDay: 0,
      lastDay: 0,
    }));
    render(<PlaceTable skew rows={tableRows} onSelect={() => undefined} selected="" />);
    const headers = screen.getAllByRole("columnheader").map((h) => h.textContent);
    expect(headers).toEqual([
      "Place",
      "State",
      "Pages published",
      "Matched",
      "Expected",
      "Relative rate",
      "90% range",
      "Languages",
    ]);
    // Sorted by relative rate, highest first.
    expect(screen.getAllByRole("rowheader").map((h) => h.textContent)).toEqual([
      "Place b",
      "Place a",
      "Place e",
      "Place c",
      "Place d",
    ]);
    expect(screen.getByText("0.60 to 1.8× (not enough pages to tell)")).toBeTruthy();
    cleanup();
    render(<StateTable rows={[{ state: "AL", skew: info(2, 1.5, 2.5) }]} />);
    expect(screen.getByRole("table", { name: "By state, each compared with the other states" })).toBeTruthy();
  });

  it("exports one CSV row per place", () => {
    const csv = skewCsv([
      { ...row("P1", info(6.14, 5.23, 7.15)), firstDay: dayNumber("1896-07-10"), lastDay: dayNumber("1896-12-28") },
      { ...row("P2", info(1, 0.5, 2, { languages: "Papers in German, Serbian and English" })), name: 'A, "B"' },
    ]);
    expect(csv.split("\n")).toEqual([
      "place_id,name,state,pages,hits,expected,estimate,lower,upper,languages,first_seen,last_seen",
      "P1,Place P1,AL,100,10,5,6.14,5.23,7.15,,1896-07-10,1896-12-28",
      'P2,"A, ""B""",AL,100,10,5,1,0.5,2,"Papers in German, Serbian and English",,',
      "",
    ]);
  });
});

describe("Clearest differences: Show more", () => {
  afterEach(cleanup);
  // `n` places clearly above 1×, the first highest, and `m` clearly below.
  const many = (n: number, m = 0): SkewRow[] => [
    ...Array.from({ length: n }, (_, i) => row(`up${i + 1}`, info(4, 3 - i / 100, 5))),
    ...Array.from({ length: m }, (_, i) => row(`down${i + 1}`, info(0.2, 0.1, 0.3 + i / 100))),
  ];
  const section = (heading: string) => screen.getByRole("heading", { name: heading }).closest("section")!;
  const above = () => section("Most clearly above 1×");
  const below = () => section("Most clearly below 1×");
  const names = (el: HTMLElement) =>
    within(within(el).getByRole("list"))
      .getAllByRole("listitem")
      .map((li) => li.querySelector("button")!.textContent);
  const button = (el: HTMLElement, name: string) => within(el).queryByRole("button", { name });

  it("shows five, says how many there are, and adds the next five", () => {
    render(<Lists rows={many(12)} />);
    expect(names(above())).toEqual([
      "Place up1, AL",
      "Place up2, AL",
      "Place up3, AL",
      "Place up4, AL",
      "Place up5, AL",
    ]);
    expect(above().textContent).toContain("Showing 5 of 12.");
    const more = button(above(), "Show more")!;
    expect(more.tagName).toBe("BUTTON");
    expect(more.getAttribute("aria-expanded")).toBe("false");
    const list = within(above()).getByRole("list");
    expect(more.getAttribute("aria-controls")).toBe(list.id);
    expect(button(above(), "Show fewer")).toBeNull();

    fireEvent.click(more);
    expect(names(above())).toHaveLength(10);
    expect(names(above())[5]).toBe("Place up6, AL");
    expect(above().textContent).toContain("Showing 10 of 12.");
    // Focus moves to the first place it added.
    expect(document.activeElement?.textContent).toBe("Place up6, AL");
    // Both buttons say the list is expanded.
    expect(button(above(), "Show more")!.getAttribute("aria-expanded")).toBe("true");
    const fewer = button(above(), "Show fewer")!;
    expect(fewer.getAttribute("aria-expanded")).toBe("true");
    expect(fewer.getAttribute("aria-controls")).toBe(list.id);

    // The last two: all of them, and no more "Show more".
    fireEvent.click(button(above(), "Show more")!);
    expect(names(above())).toHaveLength(12);
    expect(names(above()).at(-1)).toBe("Place up12, AL");
    expect(above().textContent).toContain("Showing all 12.");
    expect(button(above(), "Show more")).toBeNull();
    expect(document.activeElement?.textContent).toBe("Place up11, AL");
  });

  it("goes back to five with Show fewer, and focus stays on the list's button", () => {
    render(<Lists rows={many(12)} />);
    fireEvent.click(button(above(), "Show more")!);
    fireEvent.click(button(above(), "Show more")!);
    fireEvent.click(button(above(), "Show fewer")!);
    expect(names(above())).toHaveLength(5);
    expect(above().textContent).toContain("Showing 5 of 12.");
    expect(button(above(), "Show fewer")).toBeNull();
    expect(document.activeElement).toBe(button(above(), "Show more"));
  });

  it("keeps how many each list shows when the lists move, as on a phone rotation", () => {
    const { rerender } = render(<Lists rows={many(12, 12)} />);
    fireEvent.click(button(above(), "Show more")!);
    rerender(<Lists rows={many(12, 12)} narrow />);
    expect(names(above())).toHaveLength(10);
    expect(names(below())).toHaveLength(5);
  });

  it("offers no button when the list has five places or fewer", () => {
    render(<Lists rows={many(5, 2)} />);
    expect(names(above())).toHaveLength(5);
    expect(names(below())).toHaveLength(2);
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
    expect(screen.queryByText(/Showing/)).toBeNull();
  });

  it("expands each list on its own", () => {
    render(<Lists rows={many(8, 9)} />);
    expect(below().textContent).toContain("Showing 5 of 9.");
    fireEvent.click(button(below(), "Show more")!);
    expect(names(below())).toHaveLength(9);
    expect(names(above())).toHaveLength(5);
    expect(button(above(), "Show fewer")).toBeNull();
    expect(button(above(), "Show more")).not.toBeNull();
    fireEvent.click(button(above(), "Show more")!);
    fireEvent.click(button(below(), "Show fewer")!);
    expect(names(above())).toHaveLength(8);
    expect(names(below())).toHaveLength(5);
  });

  it("stops at 25 and points to the table", () => {
    render(<Lists rows={many(40)} />);
    for (let i = 0; i < 4; i++) fireEvent.click(button(above(), "Show more")!);
    expect(names(above())).toHaveLength(25);
    expect(above().textContent).toContain("Showing 25 of 40. The table lists every place.");
    expect(button(above(), "Show more")).toBeNull();
    expect(button(above(), "Show fewer")).not.toBeNull();
  });

  it("goes back to five when the search or window changes, not when the date moves", () => {
    const { rerender } = render(<Lists rows={many(12, 12)} resetKey="q1|null" />);
    fireEvent.click(button(above(), "Show more")!);
    fireEvent.click(button(below(), "Show more")!);
    // A new date in the same search and window: the rows change, the lists stay open.
    rerender(<Lists rows={many(11, 12)} resetKey="q1|null" />);
    expect(names(above())).toHaveLength(10);
    expect(above().textContent).toContain("Showing 10 of 11.");
    // A new window.
    rerender(<Lists rows={many(11, 12)} resetKey="q1|12" />);
    expect(names(above())).toHaveLength(5);
    expect(names(below())).toHaveLength(5);
    fireEvent.click(button(above(), "Show more")!);
    // A new search.
    rerender(<Lists rows={many(11, 12)} resetKey="q2|12" />);
    expect(names(above())).toHaveLength(5);
  });

  it("keeps the language notes working on the places it adds", () => {
    const rows = [
      ...many(6),
      row(
        "de",
        info(4, 1.5, 6, { languages: "Papers in German", languageCounts: "Of its 2 papers, 2 are in German." }),
      ),
    ];
    render(<Lists rows={rows} />);
    expect(screen.queryByText("Papers in German")).toBeNull();
    fireEvent.click(button(above(), "Show more")!);
    const tip = within(above()).getByRole("button", { name: "What this means" });
    fireEvent.click(tip);
    expect(tip.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByRole("note").textContent).toBe("Of its 2 papers, 2 are in German." + LANGUAGE_EXPLAINER);
  });
});
