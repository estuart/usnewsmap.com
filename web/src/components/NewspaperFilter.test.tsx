import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { Meta, TitleEntry } from "../api/types";
import { searchQuery } from "../api/client";
import { DEFAULTS } from "../state/url";
import {
  fold,
  locTitleUrl,
  matchTitles,
  MAX_PAPERS,
  NewspaperFilter,
  newspaperSummary,
  paperFilterNames,
  titleDetail,
  years,
} from "./NewspaperFilter";
import { SearchBar } from "./SearchBar";

afterEach(cleanup);

const paper = (lccn: string, title: string, place: string, state: string, pages: number, extra = {}): TitleEntry => ({
  lccn,
  title,
  place_id: `P-${lccn}`,
  place,
  state,
  first_year: 1871,
  last_year: 1922,
  pages,
  languages: ["eng"],
  ...extra,
});

const TITLES: TitleEntry[] = [
  paper("sn1", "The Omaha Daily Bee", "Omaha", "NE", 9000),
  paper("sn2", "The Bee", "Omaha", "NE", 4000),
  paper("sn3", "Omaha Daily Herald", "Omaha", "NE", 12000),
  paper("sn4", "The Herald", "Beeville", "TX", 100),
  paper("sn5", "L'Abeille de la Nouvelle-Orléans", "New Orleans", "LA", 20000, { languages: ["fre", "eng"] }),
  paper("sn6", "Daily Alta California", "San Francisco", "CA", 30000, { last_year: null }),
];

describe("finding newspapers (07 §7.13)", () => {
  it("folds case, accents and punctuation", () => {
    expect(fold("L'Abeille de la Nouvelle-Orléans")).toBe("l abeille de la nouvelle orleans");
  });

  it("matches every word at the start of a word of the title, place, state or LCCN", () => {
    const lccns = (text: string) => matchTitles(TITLES, text).shown.map((t) => t.lccn);
    // Titles beginning with the text first (a leading "The" aside), then
    // titles holding every word, then matches on the place; most pages first.
    expect(lccns("bee")).toEqual(["sn2", "sn1", "sn4"]);
    expect(lccns("omaha")).toEqual(["sn3", "sn1", "sn2"]);
    expect(lccns("daily omaha")).toEqual(["sn3", "sn1"]);
    expect(lccns("orleans")).toEqual(["sn5"]);
    expect(lccns("nouvelle-orléans")).toEqual(["sn5"]);
    expect(lccns("herald tx")).toEqual(["sn4"]);
    expect(lccns("sn6")).toEqual(["sn6"]);
    // Inside a word is no match: "maha" isn't Omaha.
    expect(lccns("maha")).toEqual([]);
    expect(lccns("  ")).toEqual([]);
  });

  it("lists at most the limit and counts every match", () => {
    const many = Array.from({ length: 120 }, (_, i) => paper(`sn${i}`, `Gazette ${i}`, "Town", "IA", i));
    const { shown, total } = matchTitles(many, "gazette", 50);
    expect(total).toBe(120);
    expect(shown).toHaveLength(50);
    expect(shown[0]!.lccn).toBe("sn119");
  });

  it("describes a paper by place, years, languages and pages", () => {
    expect(titleDetail(TITLES[4]!)).toBe("New Orleans, LA · 1871–1922 · French, English · 20,000 pages");
    expect(years({ first_year: 1850, last_year: null })).toBe("1850–");
    expect(years({ first_year: 1898, last_year: 1898 })).toBe("1898");
    expect(years({ first_year: null, last_year: null })).toBe("");
    expect(titleDetail({ ...TITLES[0]!, place: null, pages: null, first_year: null, last_year: null })).toBe(
      "NE · English",
    );
    expect(locTitleUrl("sn84026749")).toBe("https://www.loc.gov/item/sn84026749/");
  });

  it("summarises the choice", () => {
    const nameOf = (l: string) => TITLES.find((t) => t.lccn === l)?.title ?? l;
    expect(newspaperSummary([], nameOf)).toBe("Newspapers: any");
    expect(newspaperSummary(["sn2"], nameOf)).toBe("Newspapers: The Bee");
    expect(newspaperSummary(["sn1", "sn2"], nameOf)).toBe("Newspapers: 2 chosen");
    expect(paperFilterNames(["sn1", "sn2"], nameOf)).toEqual({ names: ["The Omaha Daily Bee", "The Bee"], more: 0 });
    expect(paperFilterNames(["sn1", "sn2", "sn3", "sn4"], nameOf)).toEqual({
      names: ["The Omaha Daily Bee", "The Bee"],
      more: 2,
    });
  });
});

describe("NewspaperFilter", () => {
  const open = () => fireEvent.click(screen.getByRole("button", { name: /^Newspapers:/ }));
  const box = () => screen.getByRole("combobox", { name: "Newspapers" });

  it("asks for the list when first opened, and waits for it", () => {
    const onWant = vi.fn();
    render(<NewspaperFilter id="t" titles={undefined} onWant={onWant} value={[]} onChange={() => {}} />);
    open();
    expect(onWant).toHaveBeenCalledTimes(1);
    expect((box() as HTMLInputElement).disabled).toBe(true);
  });

  it("chooses a match with the arrow keys and Enter, without submitting", () => {
    const onChange = vi.fn();
    const onSubmit = vi.fn((e: Event) => e.preventDefault());
    render(
      <form onSubmit={(e) => onSubmit(e.nativeEvent)}>
        <NewspaperFilter id="t" titles={TITLES} value={["sn6"]} onChange={onChange} />
      </form>,
    );
    open();
    const input = box();
    expect(input.getAttribute("aria-expanded")).toBe("false");
    fireEvent.change(input, { target: { value: "omaha" } });
    expect(input.getAttribute("aria-expanded")).toBe("true");
    const options = screen.getAllByRole("option");
    expect(options.map((o) => o.querySelector(".papers-filter__title")?.textContent)).toEqual([
      "Omaha Daily Herald",
      "The Omaha Daily Bee",
      "The Bee",
    ]);
    expect(screen.getByRole("status").textContent).toBe("3 newspapers match. Use the arrow keys to choose.");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(input.getAttribute("aria-activedescendant")).toBe(options[0]!.id);
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    // Round to the top again.
    expect(input.getAttribute("aria-activedescendant")).toBe(options[0]!.id);
    fireEvent.keyDown(input, { key: "ArrowUp" });
    expect(input.getAttribute("aria-activedescendant")).toBe(options[2]!.id);
    fireEvent.keyDown(input, { key: "Enter" });
    // Added to the papers chosen, sorted like the URL's `lccn`.
    expect(onChange).toHaveBeenLastCalledWith(["sn2", "sn6"]);
    expect(onSubmit).not.toHaveBeenCalled();
    expect((input as HTMLInputElement).value).toBe("");
    expect(input.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(input);
  });

  it("marks chosen papers, removes them, and links each to its issues at the Library of Congress", () => {
    const onChange = vi.fn();
    render(<NewspaperFilter id="t" titles={TITLES} value={["sn1", "sn5"]} onChange={onChange} />);
    expect(screen.getByRole("button", { name: "Newspapers: 2 chosen" })).toBeTruthy();
    open();
    fireEvent.change(box(), { target: { value: "bee" } });
    const listbox = screen.getByRole("listbox", { name: "Newspapers found" });
    expect(listbox.getAttribute("aria-multiselectable")).toBe("true");
    const daily = screen.getByRole("option", { name: /The Omaha Daily Bee/ });
    expect(daily.getAttribute("aria-selected")).toBe("true");
    // Choosing a chosen paper again takes it out.
    fireEvent.click(daily);
    expect(onChange).toHaveBeenLastCalledWith(["sn5"]);
    const chosen = screen.getByRole("list", { name: "Chosen newspapers" });
    const link = screen.getByRole("link", { name: "Issues of L'Abeille de la Nouvelle-Orléans at loc.gov" });
    expect(chosen.contains(link)).toBe(true);
    expect(link.getAttribute("href")).toBe("https://www.loc.gov/item/sn5/");
    expect(link.getAttribute("target")).toBe("_blank");
    fireEvent.click(screen.getByRole("button", { name: "Remove L'Abeille de la Nouvelle-Orléans" }));
    expect(onChange).toHaveBeenLastCalledWith(["sn1"]);
    fireEvent.click(screen.getByRole("button", { name: "Clear newspapers" }));
    expect(onChange).toHaveBeenLastCalledWith([]);
  });

  it("closes the list on Escape, then the filter, returning focus to its button", () => {
    render(<NewspaperFilter id="t" titles={TITLES} value={[]} onChange={() => {}} />);
    open();
    const button = screen.getByRole("button", { name: "Newspapers: any" });
    fireEvent.change(box(), { target: { value: "herald" } });
    fireEvent.keyDown(box(), { key: "Escape" });
    expect(box().getAttribute("aria-expanded")).toBe("false");
    expect(button.getAttribute("aria-expanded")).toBe("true");
    // The text goes next, then the filter closes.
    fireEvent.keyDown(box(), { key: "Escape" });
    expect((box() as HTMLInputElement).value).toBe("");
    fireEvent.keyDown(box(), { key: "Escape" });
    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(button);
  });

  it("names no more papers than a search takes", () => {
    const many = Array.from({ length: MAX_PAPERS }, (_, i) => `sx${i}`);
    const onChange = vi.fn();
    render(<NewspaperFilter id="t" titles={TITLES} value={many} onChange={onChange} />);
    open();
    fireEvent.change(box(), { target: { value: "bee" } });
    fireEvent.click(screen.getByRole("option", { name: /The Omaha Daily Bee/ }));
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByText(`A search can name at most ${MAX_PAPERS} newspapers.`)).toBeTruthy();
  });
});

describe("the newspaper filter in the search form", () => {
  const meta = {
    bounds: { from: "1736-01-01", to: "1963-12-31" },
    limits: { max_query_chars: 256 },
    ja: null,
  } as unknown as Meta;

  it("sends several papers as the search's `lccn` and counts them as one option", () => {
    const onSearch = vi.fn();
    render(
      <SearchBar view={{ ...DEFAULTS, q: "gold", lccn: ["sn6"] }} meta={meta} onSearch={onSearch} titles={TITLES} />,
    );
    expect(screen.getByRole("button", { name: /^Options/ }).textContent).toBe("Options (1)");
    fireEvent.click(screen.getByRole("button", { name: "Newspapers: Daily Alta California" }));
    const input = screen.getByRole("combobox", { name: "Newspapers" });
    fireEvent.change(input, { target: { value: "herald omaha" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.change(input, { target: { value: "beeville" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    // Enter in the box chose papers; it didn't search.
    expect(onSearch).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Newspapers: 3 chosen" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Search" }));
    expect(onSearch).toHaveBeenCalledTimes(1);
    const patch = onSearch.mock.calls[0]![0] as { lccn: string[] };
    expect(patch.lccn).toEqual(["sn3", "sn4", "sn6"]);
    expect(searchQuery({ q: "gold", lccn: patch.lccn }, "v1").get("lccn")).toBe("sn3,sn4,sn6");
  });

  it("isn't offered for a Congress search, which drops the filter", () => {
    const onSearch = vi.fn();
    const withCongress = {
      ...meta,
      congress: {
        series: [{ series: "serialset", pages: 1, from: "1817-12-01", to: "1994-12-31" }],
        place: { id: "PC001", name: "Congress", lat: 38.89, lon: -77.01 },
      },
    } as Meta;
    render(
      <SearchBar
        view={{ ...DEFAULTS, q: "gold", lccn: ["sn6"] }}
        meta={withCongress}
        onSearch={onSearch}
        titles={TITLES}
      />,
    );
    fireEvent.change(screen.getByRole("combobox", { name: "Search" }), { target: { value: "congress" } });
    expect(screen.queryByRole("button", { name: /^Newspapers:/ })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Search" }));
    expect((onSearch.mock.calls[0]![0] as { lccn: string[] }).lccn).toEqual([]);
  });
});
