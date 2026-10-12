import type { ComponentProps } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import {
  PagesLists,
  WhenLists,
  exactMedian,
  medianCandidates,
  daysFor,
  medianExtremes,
  medianLists,
  mostPages,
  statePages,
  type ListRow,
  type PlaceDays,
} from "./PlaceLists";
import { dayNumber } from "../lib/time";
import { useListLengths } from "./ListLength";

beforeEach(() => window.localStorage.clear());
afterEach(cleanup);

const row = (id: string, state: string, value: number, when = NaN, whenLabel = ""): ListRow => ({
  id,
  name: id,
  state,
  value,
  when,
  whenLabel,
});

// The Pages lists as App renders them, with how many each shows kept above them.
// `narrow` moves them, as a phone rotation does, which remounts them.
function Pages({
  rows,
  onSelect = () => undefined,
  trailing = false,
  resetKey = "",
  narrow = false,
}: {
  rows: ListRow[];
  onSelect?: (id: string) => void;
  trailing?: boolean;
  resetKey?: string;
  narrow?: boolean;
}) {
  const [shown, setShown] = useListLengths(resetKey, ["places", "states"]);
  const lists = <PagesLists rows={rows} onSelect={onSelect} trailing={trailing} shown={shown} onShown={setShown} />;
  return narrow ? <div>{lists}</div> : <section>{lists}</section>;
}

// The Median date lists as App renders them, as `Pages` above.
function When({
  resetKey = "",
  narrow = false,
  ...props
}: Omit<ComponentProps<typeof WhenLists>, "shown" | "onShown"> & { resetKey?: string; narrow?: boolean }) {
  const [shown, setShown] = useListLengths(resetKey, ["earliest", "latest"]);
  const lists = <WhenLists {...props} shown={shown} onShown={setShown} />;
  return narrow ? <div>{lists}</div> : <section>{lists}</section>;
}

const ROWS: ListRow[] = [
  row("Chicago", "IL", 500, 0.6, "Jun 1896"),
  row("Peoria", "IL", 40, 0.2, "Feb 1880"),
  row("Albany", "NY", 300, 0.9, "Oct 1910"),
  row("Buffalo", "NY", 300, 0.1, "Jan 1875"),
  row("Reno", "NV", 4, 0.0, "Jan 1870"),
  row("Tucson", "AZ", 20, 0.5, "Jan 1895"),
  row("Boise", "ID", 10, 0.4, "Jan 1890"),
  row("Empty", "OR", 0),
];

describe("the lists", () => {
  it("ranks places and states by pages, ties by name, and skips places with none", () => {
    // All of them, for the list to cut.
    expect(mostPages(ROWS).map((r) => r.id)).toEqual([
      "Chicago",
      "Albany",
      "Buffalo",
      "Peoria",
      "Tucson",
      "Boise",
      "Reno",
    ]);
    expect(statePages(ROWS).slice(0, 2)).toEqual([
      { state: "NY", pages: 600, places: 2 },
      { state: "IL", pages: 540, places: 2 },
    ]);
    expect(statePages(ROWS).some((s) => s.state === "OR")).toBe(false);
  });

  it("lists earliest and latest median dates among places with enough pages, never one in both", () => {
    const { earliest, latest, eligible } = medianExtremes(ROWS);
    // Reno has 4 pages, under the minimum of 5; Empty has none.
    expect(eligible).toBe(6);
    expect(earliest.map((r) => r.id)).toEqual(["Buffalo", "Peoria", "Boise"]);
    expect(latest.map((r) => r.id)).toEqual(["Albany", "Chicago", "Tucson"]);
  });
});

describe("ties and windows", () => {
  it("never lists a place in both lists, even when every median is the same", () => {
    const same = ["A", "B", "C", "D", "E", "F"].map((id, i) => row(id, "IL", 10 + i, 0.5, "Jun 1896"));
    const { earliest, latest } = medianExtremes(same);
    expect(earliest).toHaveLength(3);
    expect(latest).toHaveLength(3);
    expect(earliest.filter((r) => latest.includes(r))).toEqual([]);
  });

  it("says when the counts are a trailing window", () => {
    render(<Pages rows={[]} trailing={true} />);
    expect(screen.getAllByText("No matching pages in the playback window.")).toHaveLength(2);
    cleanup();
    render(<When rows={ROWS} onSelect={() => {}} trailing={true} />);
    expect(screen.getByText(/half of its matching pages in the playback window had been printed/)).toBeTruthy();
  });
});

describe("PagesLists", () => {
  it("names each place's pages and share, and selects a place", () => {
    const onSelect = vi.fn();
    render(<Pages rows={ROWS} onSelect={onSelect} />);
    const panel = screen.getByRole("complementary", { name: "Places and states with the most pages" });
    expect(within(panel).getByRole("heading", { level: 2, name: "Most pages" })).toBeTruthy();
    expect(panel.textContent).toContain("500 pages (42.6%)");
    expect(panel.textContent).toContain("600 pages (51.1%) in 2 places");
    fireEvent.click(within(panel).getByRole("button", { name: "Chicago, IL" }));
    expect(onSelect).toHaveBeenCalledWith("Chicago");
  });
});

describe("WhenLists", () => {
  it("shows each place's median date and says when none qualify", () => {
    render(<When rows={ROWS} onSelect={() => {}} trailing={false} />);
    expect(screen.getByText("Jan 1875 · 300 pages")).toBeTruthy();
    cleanup();
    render(<When rows={[row("Reno", "NV", 4, 0, "Jan 1870")]} onSelect={() => {}} trailing={false} />);
    expect(screen.getAllByText("No place has 5 or more matching pages up to the current date.")).toHaveLength(2);
  });
});

describe("hiding the lists", () => {
  it("hides and shows the panel, and remembers it for the next visit", () => {
    render(<Pages rows={ROWS} />);
    const hide = screen.getByRole("button", { name: "Hide" });
    expect(hide.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(hide);
    expect(screen.queryByRole("heading", { name: "Most pages" })).toBeNull();
    const show = screen.getByRole("button", { name: "Show most pages" });
    expect(show.getAttribute("aria-expanded")).toBe("false");
    expect(window.localStorage.getItem("usnm.lists")).toBe("hidden");

    // Hidden in one measure, hidden in the others too, and after a reload.
    cleanup();
    render(<When rows={ROWS} onSelect={() => {}} trailing={false} />);
    expect(screen.getByRole("button", { name: "Show earliest and latest" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Show earliest and latest" }));
    expect(screen.getByRole("heading", { name: "Earliest and latest" })).toBeTruthy();
    expect(window.localStorage.getItem("usnm.lists")).toBeNull();
  });
});

describe("exact median days", () => {
  it("asks for the places that could make either list, ties at the fifth included", () => {
    // Six eligible places: all of them could be in one list or the other.
    expect(medianCandidates(ROWS)).toEqual(["Albany", "Boise", "Buffalo", "Chicago", "Peoria", "Tucson"]);
    // Twelve places in one month: any could be first, so all twelve.
    const same = (n: number) =>
      Array.from({ length: n }, (_, i) => row(`P${String(i).padStart(2, "0")}`, "IL", 10 + i, 0.5, "Jun 1896"));
    expect(medianCandidates(same(12))).toHaveLength(12);
    // More than one request takes: none, so the lists keep the month.
    expect(medianCandidates(same(21))).toBeNull();
  });

  it("finds the first day by which half the window's pages had been printed", () => {
    const days = [100, 105, 110, 120];
    const hits = [1, 1, 2, 4];
    expect(exactMedian(days, hits, -Infinity, Infinity)).toBe(110); // 8 pages: the 4th is on day 110
    expect(exactMedian(days, hits, 105, 115)).toBe(110); // 3 pages in the window: half (1.5) by day 110
    expect(exactMedian(days, hits, 106, 109)).toBeNaN();
  });

  it("ranks and labels by the exact day once it is known", () => {
    const rows = [row("Early", "IL", 10, 0.1, "Jan 1895"), row("Later", "IL", 10, 0.1, "Jan 1895")];
    const exact = new Map([
      ["Early", dayNumber("1895-01-03")],
      ["Later", dayNumber("1895-01-28")],
    ]);
    render(<When rows={rows} onSelect={() => {}} trailing={false} exact={exact} />);
    const earliest = screen.getByRole("heading", { name: "Earliest median date" }).parentElement!;
    expect(earliest.textContent).toContain("Early, IL Jan 3, 1895 · 10 pages");
    const latest = screen.getByRole("heading", { name: "Latest median date" }).parentElement!;
    expect(latest.textContent).toContain("Later, IL Jan 28, 1895 · 10 pages");
  });
});

describe("WhenLists while the exact days load", () => {
  const rows = [row("Early", "IL", 10, 0.1, "Jan 1895"), row("Later", "IL", 10, 0.1, "Jan 1895")];

  it("says it is loading and lists nothing (no month-level list first)", () => {
    render(<When rows={rows} onSelect={() => {}} trailing={false} status="loading" />);
    expect(screen.getByRole("status").textContent).toBe("Finding each place's median day…");
    expect(screen.queryByRole("heading", { name: "Earliest median date" })).toBeNull();
    expect(screen.queryByText(/Jan 1895/)).toBeNull();
  });

  it("keeps showing exactly the last exact lists while updating, then the fresh ones", () => {
    const first = new Map([
      ["Early", dayNumber("1895-01-03")],
      ["Later", dayNumber("1895-01-28")],
    ]);
    const { rerender } = render(<When rows={rows} onSelect={() => {}} trailing={false} exact={first} status="exact" />);
    expect(screen.getByText("Jan 3, 1895 · 10 pages")).toBeTruthy();
    // The window moved: new rows and the old answer recomputed differently, but nothing changes yet.
    const moved = [row("Early", "IL", 30, 0.1, "Jan 1895"), row("Later", "IL", 30, 0.1, "Jan 1895")];
    const recomputed = new Map([["Early", dayNumber("1895-01-20")]]);
    rerender(<When rows={moved} onSelect={() => {}} trailing={false} exact={recomputed} status="updating" />);
    expect(screen.getByText("Jan 3, 1895 · 10 pages")).toBeTruthy();
    expect(screen.getByText("Jan 28, 1895 · 10 pages")).toBeTruthy();
    // The fresh answer replaces them.
    const fresh = new Map([
      ["Early", dayNumber("1895-01-05")],
      ["Later", dayNumber("1895-01-29")],
    ]);
    rerender(<When rows={moved} onSelect={() => {}} trailing={false} exact={fresh} status="exact" />);
    expect(screen.getByText("Jan 5, 1895 · 30 pages")).toBeTruthy();
    expect(screen.queryByText("Updating for the new date…")).toBeNull();
  });

  it("keeps the last exact lists, marked as updating", () => {
    const exact = new Map([
      ["Early", dayNumber("1895-01-03")],
      ["Later", dayNumber("1895-01-28")],
    ]);
    render(<When rows={rows} onSelect={() => {}} trailing={false} exact={exact} status="updating" />);
    expect(screen.getByText("Jan 3, 1895 · 10 pages")).toBeTruthy();
    expect(screen.getByText("Updating for the new date…")).toBeTruthy();
    expect(screen.queryByText(/Jan 1895/)).toBeNull();
  });

  it("ranks by bucket only when that is final", () => {
    render(<When rows={rows} onSelect={() => {}} trailing={false} status="bucket" />);
    expect(screen.getAllByText("Jan 1895 · 10 pages")).toHaveLength(2);
    expect(screen.queryByText("Updating for the new date…")).toBeNull();
  });
});

describe("Most pages: Show more", () => {
  // `n` places with pages, the first most, in `s` states, the first most.
  const many = (n: number, s = 1): ListRow[] =>
    Array.from({ length: n }, (_, i) => row(`P${i + 1}`, `S${(i % s) + 1}`, 1000 - i));
  const section = (heading: string) => screen.getByRole("heading", { name: heading }).closest("section")!;
  const places = () => section("Places");
  const states = () => section("States");
  const entries = (el: HTMLElement) => within(within(el).getByRole("list")).getAllByRole("listitem");
  const button = (el: HTMLElement, name: string) => within(el).queryByRole("button", { name });

  it("shows five places, says how many there are, adds five, and goes back", () => {
    render(<Pages rows={many(12)} />);
    expect(entries(places()).map((li) => li.querySelector("button")!.textContent)).toEqual([
      "P1, S1",
      "P2, S1",
      "P3, S1",
      "P4, S1",
      "P5, S1",
    ]);
    expect(places().textContent).toContain("Showing 5 of 12.");
    const more = button(places(), "Show more")!;
    const list = within(places()).getByRole("list");
    expect(more.getAttribute("aria-expanded")).toBe("false");
    expect(more.getAttribute("aria-controls")).toBe(list.id);
    expect(button(places(), "Show fewer")).toBeNull();

    fireEvent.click(more);
    expect(entries(places())).toHaveLength(10);
    expect(places().textContent).toContain("Showing 10 of 12.");
    // Focus moves to the first place it added.
    expect(document.activeElement?.textContent).toBe("P6, S1");
    expect(button(places(), "Show more")!.getAttribute("aria-expanded")).toBe("true");
    const fewer = button(places(), "Show fewer")!;
    expect(fewer.getAttribute("aria-expanded")).toBe("true");
    expect(fewer.getAttribute("aria-controls")).toBe(list.id);

    fireEvent.click(button(places(), "Show more")!);
    expect(places().textContent).toContain("Showing all 12.");
    expect(button(places(), "Show more")).toBeNull();

    fireEvent.click(button(places(), "Show fewer")!);
    expect(entries(places())).toHaveLength(5);
    expect(places().textContent).toContain("Showing 5 of 12.");
    expect(document.activeElement).toBe(button(places(), "Show more"));
  });

  it("expands each list on its own, and focuses the first state it adds, which has no button", () => {
    render(<Pages rows={many(30, 8)} />);
    expect(states().textContent).toContain("Showing 5 of 8.");
    fireEvent.click(button(states(), "Show more")!);
    expect(entries(states())).toHaveLength(8);
    expect(states().textContent).toContain("Showing all 8.");
    expect(document.activeElement).toBe(entries(states())[5]);
    expect(document.activeElement?.textContent).toMatch(/^S6 /);
    // The places list didn't move.
    expect(entries(places())).toHaveLength(5);
    expect(button(places(), "Show fewer")).toBeNull();
  });

  it("offers no button with five or fewer", () => {
    render(<Pages rows={many(5, 2)} />);
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
    expect(screen.queryByText(/Showing/)).toBeNull();
  });

  it("stops at 25 and points to the table", () => {
    render(<Pages rows={many(40)} />);
    for (let i = 0; i < 4; i++) fireEvent.click(button(places(), "Show more")!);
    expect(entries(places())).toHaveLength(25);
    expect(places().textContent).toContain("Showing 25 of 40. The table lists every place.");
    expect(button(places(), "Show more")).toBeNull();
    expect(button(places(), "Show fewer")).not.toBeNull();
  });

  it("keeps how many each list shows when the lists move, and resets for a new search or window", () => {
    const { rerender } = render(<Pages rows={many(30, 8)} resetKey="q1|null" />);
    fireEvent.click(button(places(), "Show more")!);
    fireEvent.click(button(states(), "Show more")!);
    rerender(<Pages rows={many(30, 8)} resetKey="q1|null" narrow />);
    expect(entries(places())).toHaveLength(10);
    expect(entries(states())).toHaveLength(8);
    // A new date in the same search and window keeps them.
    rerender(<Pages rows={many(29, 8)} resetKey="q1|null" narrow />);
    expect(entries(places())).toHaveLength(10);
    // A new window.
    rerender(<Pages rows={many(29, 8)} resetKey="q1|12" narrow />);
    expect(entries(places())).toHaveLength(5);
    expect(entries(states())).toHaveLength(5);
  });
});

// `n` eligible places, M0 earliest to M(n-1) latest, each in its own year.
const years = (n: number): ListRow[] =>
  Array.from({ length: n }, (_, i) => row(`M${i}`, "IL", 10, i / n, `${1870 + i}`));
const ids = (from: number, to: number) => Array.from({ length: to - from }, (_, i) => `M${from + i}`);

describe("Median date: Show more", () => {
  const section = (heading: string) => screen.getByRole("heading", { name: heading }).closest("section")!;
  const earliest = () => section("Earliest median date");
  const latest = () => section("Latest median date");
  const entries = (el: HTMLElement) => within(within(el).getByRole("list")).getAllByRole("listitem");
  const names = (el: HTMLElement) => entries(el).map((li) => li.querySelector("button")!.textContent);
  const button = (el: HTMLElement, name: string) => within(el).queryByRole("button", { name });

  it("splits every eligible place into the earlier and later half, none in both", () => {
    const { earliest: early, latest: late, eligible } = medianExtremes(years(31));
    expect(eligible).toBe(31);
    expect(early.map((r) => r.id)).toEqual(ids(0, 16));
    expect(late.map((r) => r.id)).toEqual(ids(16, 31).reverse());
    // Every median the same: still split, more pages first in both.
    const same = Array.from({ length: 40 }, (_, i) => row(`S${i}`, "IL", 100 - i, 0.5, "1896"));
    const tied = medianExtremes(same);
    expect(tied.earliest).toHaveLength(20);
    expect(tied.latest).toHaveLength(20);
    expect(tied.earliest.filter((r) => tied.latest.includes(r))).toEqual([]);
    expect(tied.latest[0]!.id).toBe("S20");
  });

  it("shows five of each half, adds five, and goes back", () => {
    render(<When rows={years(30)} onSelect={() => {}} trailing={false} />);
    expect(names(earliest())).toEqual(ids(0, 5).map((id) => `${id}, IL`));
    expect(names(latest())).toEqual(
      ids(25, 30)
        .reverse()
        .map((id) => `${id}, IL`),
    );
    expect(earliest().textContent).toContain("Showing 5 of 15.");
    expect(latest().textContent).toContain("Showing 5 of 15.");
    const more = button(earliest(), "Show more")!;
    const list = within(earliest()).getByRole("list");
    expect(more.getAttribute("aria-expanded")).toBe("false");
    expect(more.getAttribute("aria-controls")).toBe(list.id);

    fireEvent.click(more);
    expect(names(earliest())).toEqual(ids(0, 10).map((id) => `${id}, IL`));
    expect(earliest().textContent).toContain("Showing 10 of 15.");
    expect(document.activeElement?.textContent).toBe("M5, IL");
    expect(button(earliest(), "Show more")!.getAttribute("aria-expanded")).toBe("true");
    expect(button(earliest(), "Show fewer")!.getAttribute("aria-controls")).toBe(list.id);
    // Latest didn't move.
    expect(entries(latest())).toHaveLength(5);
    expect(button(latest(), "Show fewer")).toBeNull();

    fireEvent.click(button(earliest(), "Show more")!);
    expect(earliest().textContent).toContain("Showing all 15.");
    expect(button(earliest(), "Show more")).toBeNull();
    fireEvent.click(button(earliest(), "Show fewer")!);
    expect(entries(earliest())).toHaveLength(5);
    expect(document.activeElement).toBe(button(earliest(), "Show more"));
  });

  it("stops at 25 and points to the table, and offers no button with ten places or fewer", () => {
    render(<When rows={years(60)} onSelect={() => {}} trailing={false} />);
    for (let i = 0; i < 4; i++) fireEvent.click(button(latest(), "Show more")!);
    expect(entries(latest())).toHaveLength(25);
    expect(latest().textContent).toContain("Showing 25 of 30. The table lists every place.");
    expect(button(latest(), "Show more")).toBeNull();
    cleanup();
    render(<When rows={years(10)} onSelect={() => {}} trailing={false} />);
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
    expect(screen.queryByText(/Showing/)).toBeNull();
  });

  it("keeps how many each list shows when the lists move, and resets for a new search or window", () => {
    const { rerender } = render(<When rows={years(40)} onSelect={() => {}} trailing={false} resetKey="q1|null" />);
    fireEvent.click(button(earliest(), "Show more")!);
    fireEvent.click(button(latest(), "Show more")!);
    fireEvent.click(button(latest(), "Show more")!);
    rerender(<When rows={years(40)} onSelect={() => {}} trailing={false} resetKey="q1|null" narrow />);
    expect(entries(earliest())).toHaveLength(10);
    expect(entries(latest())).toHaveLength(15);
    rerender(<When rows={years(40)} onSelect={() => {}} trailing={false} resetKey="q1|12" narrow />);
    expect(entries(earliest())).toHaveLength(5);
    expect(entries(latest())).toHaveLength(5);
  });

  it("says the days of the places a list adds are on their way", () => {
    const rows = years(30);
    const days = (from: number, to: number) =>
      ids(from, to).map((id, i): [string, number] => [id, dayNumber(`${1870 + from + i}-03-01`)]);
    const first = new Map([...days(0, 5), ...days(25, 30)]);
    const { rerender } = render(<When rows={rows} onSelect={() => {}} trailing={false} exact={first} status="exact" />);
    expect(earliest().textContent).toContain("Mar 1, 1870");
    // In App the longer list's places are a new set, so the lists are
    // updating as soon as it shows; here they are marked so first.
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={first} status="updating" />);
    expect(screen.getByText("Updating for the new date…")).toBeTruthy();
    fireEvent.click(button(earliest(), "Show more")!);
    expect(screen.getByText("Finding the listed places' median days…")).toBeTruthy();
    const more = new Map([...days(0, 10), ...days(25, 30)]);
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={more} status="exact" />);
    expect(entries(earliest())).toHaveLength(10);
    expect(earliest().textContent).toContain("Mar 1, 1879");
    expect(screen.queryByText("Finding the listed places' median days…")).toBeNull();
  });

  // Thirty places; M5 to M14 share 1875. The first exact answer has days for
  // M0-M4 and M25-M29, so 1875 goes by bucket (by name: M10-M14, then M5-M9).
  // With days for all of 1875, M5-M9 come first and M10-M14 go past ten.
  const shared = () => years(30).map((r, i) => (i >= 5 && i < 15 ? { ...r, when: 5 / 30, whenLabel: "1875" } : r));
  const day = (i: number) => dayNumber(`${1870 + i}-03-01`);
  const firstDays = () =>
    new Map([...[0, 1, 2, 3, 4, 25, 26, 27, 28, 29].map((i): [string, number] => [`M${i}`, day(i)])]);
  const allDays = () =>
    new Map([
      ...firstDays(),
      ...ids(5, 15).map((id, i): [string, number] => [id, dayNumber(`1875-01-${String(i + 1).padStart(2, "0")}`)]),
    ]);

  it("waits for the added places' days before moving focus, so it isn't lost when they re-rank", () => {
    const rows = shared();
    const { rerender } = render(
      <When rows={rows} onSelect={() => {}} trailing={false} exact={firstDays()} status="exact" />,
    );
    // In App the longer list's places are a new set, so the lists are updating as soon as it shows.
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={firstDays()} status="updating" />);
    const more = button(earliest(), "Show more")!;
    more.focus();
    fireEvent.click(more);
    // Shown by bucket for now; focus stays on Show more rather than on M10, which is about to move.
    expect(names(earliest()).slice(5)).toEqual(["M10, IL", "M11, IL", "M12, IL", "M13, IL", "M14, IL"]);
    expect(document.activeElement).toBe(more);
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={allDays()} status="exact" />);
    expect(names(earliest()).slice(5)).toEqual(["M5, IL", "M6, IL", "M7, IL", "M8, IL", "M9, IL"]);
    // The first added place in the final order, not the body.
    expect(document.activeElement?.textContent).toBe("M5, IL");
  });

  it("moves focus to the first added place when the days fail, and not when focus has moved on", () => {
    const rows = shared();
    const { rerender } = render(
      <When rows={rows} onSelect={() => {}} trailing={false} exact={firstDays()} status="exact" />,
    );
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={firstDays()} status="updating" />);
    const more = button(earliest(), "Show more")!;
    more.focus();
    fireEvent.click(more);
    expect(document.activeElement).toBe(more);
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} status="bucket" />);
    expect(document.activeElement?.textContent).toBe(names(earliest())[5]);

    cleanup();
    const again = render(<When rows={rows} onSelect={() => {}} trailing={false} exact={firstDays()} status="exact" />);
    again.rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={firstDays()} status="updating" />);
    const more2 = button(earliest(), "Show more")!;
    more2.focus();
    fireEvent.click(more2);
    // The reader tabs on to Latest's Show more while the days load: focus stays there.
    const elsewhere = button(latest(), "Show more")!;
    elsewhere.focus();
    again.rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={allDays()} status="exact" />);
    expect(document.activeElement).toBe(elsewhere);
  });

  it("keeps lists shown by bucket, not older exact ones, while their first days load", () => {
    const rows = shared();
    const { rerender } = render(
      <When rows={rows} onSelect={() => {}} trailing={false} exact={allDays()} status="exact" />,
    );
    fireEvent.click(button(earliest(), "Show more")!);
    expect(names(earliest()).slice(5)).toEqual(["M5, IL", "M6, IL", "M7, IL", "M8, IL", "M9, IL"]);
    // Then by bucket (in App: too many tied places, maybe another search), then their days are asked for.
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} status="bucket" tied="year" />);
    rerender(<When rows={rows} onSelect={() => {}} trailing={false} exact={null} status="updating" />);
    expect(names(earliest()).slice(5)).toEqual(["M10, IL", "M11, IL", "M12, IL", "M13, IL", "M14, IL"]);
    expect(screen.queryByText("Finding each place's median day…")).toBeNull();
  });

  it("says when too many places share a bucket to rank them by day", () => {
    render(<When rows={years(30)} onSelect={() => {}} trailing={false} status="bucket" tied="month" />);
    expect(screen.getByText(/Too many places share a median month/)).toBeTruthy();
  });
});

describe("exact median days at longer lengths", () => {
  const sorted = (a: string[]) => [...a].sort();

  it("asks for the places as far down as each list shows", () => {
    expect(medianCandidates(years(100), { earliest: 10, latest: 5 })).toEqual(sorted([...ids(0, 10), ...ids(95, 100)]));
    expect(medianCandidates(years(100), { earliest: 25, latest: 25 })).toEqual(
      sorted([...ids(0, 25), ...ids(75, 100)]),
    );
    // Twelve places: Earliest holds six, all shown at ten; Latest's first
    // five. The seventh is in neither shown part.
    expect(medianCandidates(years(12), { earliest: 10, latest: 5 })).toEqual(sorted([...ids(0, 6), ...ids(7, 12)]));
  });

  it("includes every place sharing the last shown one's bucket, within one request per five shown", () => {
    // The first twelve share a year.
    const rows = years(100).map((r, i) => (i < 12 ? { ...r, when: 0 } : r));
    expect(medianCandidates(rows)).toEqual(sorted([...ids(0, 12), ...ids(95, 100)]));
    // Forty in one month: too many for the first five of each, not for 25 of each.
    const same = Array.from({ length: 40 }, (_, i) => row(`S${String(i).padStart(2, "0")}`, "IL", 10, 0.5, "1896"));
    expect(medianCandidates(same)).toBeNull();
    expect(medianCandidates(same, { earliest: 10, latest: 5 })).toBeNull();
    expect(medianCandidates(same, { earliest: 25, latest: 25 })).toHaveLength(40);
  });

  it("ranks the ends by day and the places between by bucket, counting every place", () => {
    // The first six share 1870; days come for them and the last five.
    const rows = years(30).map((r, i) => (i < 6 ? { ...r, when: 0, whenLabel: "1870" } : r));
    const exact = new Map([
      ...ids(0, 6).map((id, i): [string, number] => [id, dayNumber(`1870-01-${String(26 - 5 * i).padStart(2, "0")}`)]),
      ...ids(25, 30).map((id, i): [string, number] => [id, dayNumber(`${1895 + i}-06-01`)]),
    ]);
    expect(medianCandidates(rows)).toEqual(sorted([...exact.keys()]));
    const lists = medianLists(rows, exact);
    expect(lists.eligible).toBe(30);
    expect(lists.earliest).toHaveLength(15);
    expect(lists.earliest.slice(0, 7).map((r) => [r.id, r.whenLabel])).toEqual([
      ["M5", "Jan 1, 1870"],
      ["M4", "Jan 6, 1870"],
      ["M3", "Jan 11, 1870"],
      ["M2", "Jan 16, 1870"],
      ["M1", "Jan 21, 1870"],
      ["M0", "Jan 26, 1870"],
      ["M6", "1876"],
    ]);
    expect(lists.latest.slice(0, 6).map((r) => r.whenLabel)).toEqual([
      "Jun 1, 1899",
      "Jun 1, 1898",
      "Jun 1, 1897",
      "Jun 1, 1896",
      "Jun 1, 1895",
      "1894",
    ]);
    // Days for only part of a shared year: a place without one could be earlier, so that year goes by bucket.
    const part = new Map(ids(0, 4).map((id): [string, number] => [id, dayNumber("1870-01-01")]));
    expect(medianLists(rows, part).earliest[0]!.whenLabel).toBe("1870");
  });

  it("asks for at most 20 places at a time, and only for places not asked for before", async () => {
    const known = new Map<string, PlaceDays>();
    const ask = vi.fn(async (want: string[]) => ({
      index_version: "v1",
      // The API leaves out a place with no matching days.
      places: want.filter((id) => id !== "M3").map((id) => ({ id, days: [1], hits: [1] })),
    }));
    const first = await daysFor(ids(0, 45), known, ask);
    expect(ask.mock.calls.map(([want]) => want.length)).toEqual([20, 20, 5]);
    expect(first).toHaveLength(45);
    expect(first[3]).toEqual({ id: "M3", days: [], hits: [] });
    ask.mockClear();
    await daysFor(ids(40, 50), known, ask);
    expect(ask.mock.calls).toEqual([[ids(45, 50)]]);
    ask.mockClear();
    await daysFor(ids(0, 10), known, ask);
    expect(ask).not.toHaveBeenCalled();
  });
});
