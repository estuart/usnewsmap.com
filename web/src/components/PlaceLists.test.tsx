import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import {
  PagesLists,
  WhenLists,
  exactMedian,
  medianCandidates,
  medianExtremes,
  mostPages,
  statePages,
  type ListRow,
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
    render(<WhenLists rows={ROWS} onSelect={() => {}} trailing={true} />);
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
    render(<WhenLists rows={ROWS} onSelect={() => {}} trailing={false} />);
    expect(screen.getByText("Jan 1875 · 300 pages")).toBeTruthy();
    cleanup();
    render(<WhenLists rows={[row("Reno", "NV", 4, 0, "Jan 1870")]} onSelect={() => {}} trailing={false} />);
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
    render(<WhenLists rows={ROWS} onSelect={() => {}} trailing={false} />);
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
    expect(medianCandidates(same(21))).toEqual([]);
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
    render(<WhenLists rows={rows} onSelect={() => {}} trailing={false} exact={exact} />);
    const earliest = screen.getByRole("heading", { name: "Earliest median date" }).parentElement!;
    expect(earliest.textContent).toContain("Early, IL Jan 3, 1895 · 10 pages");
    const latest = screen.getByRole("heading", { name: "Latest median date" }).parentElement!;
    expect(latest.textContent).toContain("Later, IL Jan 28, 1895 · 10 pages");
  });
});

describe("WhenLists while the exact days load", () => {
  const rows = [row("Early", "IL", 10, 0.1, "Jan 1895"), row("Later", "IL", 10, 0.1, "Jan 1895")];

  it("says it is loading and lists nothing (no month-level list first)", () => {
    render(<WhenLists rows={rows} onSelect={() => {}} trailing={false} status="loading" />);
    expect(screen.getByRole("status").textContent).toBe("Finding each place's median day…");
    expect(screen.queryByRole("heading", { name: "Earliest median date" })).toBeNull();
    expect(screen.queryByText(/Jan 1895/)).toBeNull();
  });

  it("keeps showing exactly the last exact lists while updating, then the fresh ones", () => {
    const first = new Map([
      ["Early", dayNumber("1895-01-03")],
      ["Later", dayNumber("1895-01-28")],
    ]);
    const { rerender } = render(
      <WhenLists rows={rows} onSelect={() => {}} trailing={false} exact={first} status="exact" />,
    );
    expect(screen.getByText("Jan 3, 1895 · 10 pages")).toBeTruthy();
    // The window moved: new rows and the old answer recomputed differently, but nothing changes yet.
    const moved = [row("Early", "IL", 30, 0.1, "Jan 1895"), row("Later", "IL", 30, 0.1, "Jan 1895")];
    const recomputed = new Map([["Early", dayNumber("1895-01-20")]]);
    rerender(<WhenLists rows={moved} onSelect={() => {}} trailing={false} exact={recomputed} status="updating" />);
    expect(screen.getByText("Jan 3, 1895 · 10 pages")).toBeTruthy();
    expect(screen.getByText("Jan 28, 1895 · 10 pages")).toBeTruthy();
    // The fresh answer replaces them.
    const fresh = new Map([
      ["Early", dayNumber("1895-01-05")],
      ["Later", dayNumber("1895-01-29")],
    ]);
    rerender(<WhenLists rows={moved} onSelect={() => {}} trailing={false} exact={fresh} status="exact" />);
    expect(screen.getByText("Jan 5, 1895 · 30 pages")).toBeTruthy();
    expect(screen.queryByText("Updating for the new date…")).toBeNull();
  });

  it("keeps the last exact lists, marked as updating", () => {
    const exact = new Map([
      ["Early", dayNumber("1895-01-03")],
      ["Later", dayNumber("1895-01-28")],
    ]);
    render(<WhenLists rows={rows} onSelect={() => {}} trailing={false} exact={exact} status="updating" />);
    expect(screen.getByText("Jan 3, 1895 · 10 pages")).toBeTruthy();
    expect(screen.getByText("Updating for the new date…")).toBeTruthy();
    expect(screen.queryByText(/Jan 1895/)).toBeNull();
  });

  it("ranks by bucket only when that is final", () => {
    render(<WhenLists rows={rows} onSelect={() => {}} trailing={false} status="bucket" />);
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

  it("leaves the Median date lists at five, with no buttons", () => {
    const rows = Array.from({ length: 30 }, (_, i) => row(`M${i}`, "IL", 10, i / 30, `Jan ${1870 + i}`));
    render(<WhenLists rows={rows} onSelect={() => {}} trailing={false} />);
    expect(entries(section("Earliest median date"))).toHaveLength(5);
    expect(entries(section("Latest median date"))).toHaveLength(5);
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
    expect(screen.queryByText(/Showing/)).toBeNull();
  });
});
