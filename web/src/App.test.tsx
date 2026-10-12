import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { AggregateResponse, DaysResponse, HitsResponse, Meta, PlacesResponse, TitlesResponse } from "./api/types";
import { dayNumber } from "./lib/time";

// The API answers from these, so App can be shown for a search (no WebGL in
// jsdom: it shows the tables, not the map).
const { api } = vi.hoisted(() => ({
  api: {
    meta: vi.fn(),
    places: vi.fn(),
    titles: vi.fn(),
    aggregate: vi.fn(),
    coverage: vi.fn(),
    days: vi.fn(),
    hits: vi.fn(),
  },
}));
vi.mock("./api/client", async (importOriginal) => ({ ...(await importOriginal<object>()), api }));
// jsdom has no canvas: say so quietly, as a browser without WebGL2 would.
HTMLCanvasElement.prototype.getContext = (() => null) as typeof HTMLCanvasElement.prototype.getContext;

const { App } = await import("./App");

const meta = {
  index_version: "v1",
  bounds: { from: "1836-01-01", to: "1963-12-31" },
  synthetic: false,
  titles: 1,
  limits: { max_query_chars: 256, max_slop: 20 },
  ja: null,
  congress: {
    series: [{ series: "globe", pages: 10, from: "1833-12-02", to: "1873-03-03" }],
    place: { id: "PC001", name: "Congress", lat: 38.89, lon: -77.01 },
  },
} as unknown as Meta;

const places: PlacesResponse = {
  type: "FeatureCollection",
  index_version: "v1",
  features: [
    {
      type: "Feature",
      id: "P00001",
      geometry: { type: "Point", coordinates: [-84.39, 33.75] },
      properties: { name: "Atlanta", state: "GA", precision: "city", titles: 1 },
    },
  ],
};

/** One year's answer: `hits` pages at `place`, from the newspaper `lccn` unless it is empty. */
function aggregate(place: string, hits: number, lccn: string): AggregateResponse {
  return {
    index_version: "v1",
    synthetic: false,
    query: { canonical: "gold", ast: "" },
    bucket: { unit: "year", from: "1850-01-01", to: "1850-12-31", count: 1 },
    coarsened: false,
    total: { hits, places: hits > 0 ? 1 : 0, baseline_pages: 0, papers: lccn ? 1 : 0 },
    series: { hits: [hits], baseline: null },
    places: hits > 0 ? { id: [place], hits: [hits], first_day: [54787] } : { id: [], hits: [], first_day: [] },
    cube: { p: hits > 0 ? [0] : [], b: hits > 0 ? [0] : [], h: hits > 0 ? [hits] : [], calls: 1, baseline_ref: null },
    papers: lccn
      ? { lccn: [lccn], hits: [hits], title: ["The Atlanta Intelligencer"], place_id: [place] }
      : { lccn: [], hits: [], title: [], place_id: [] },
    baseline: null,
  };
}

const titles: TitlesResponse = {
  index_version: "v1",
  titles: [
    {
      lccn: "sn82014424",
      title: "The Atlanta Intelligencer",
      place_id: "P00001",
      place: "Atlanta",
      state: "GA",
      first_year: 1849,
      last_year: 1871,
      pages: 2000,
      first_page_date: "1860-01-04",
      last_page_date: "1871-12-28",
      languages: ["eng"],
    },
    {
      lccn: "sn89053729",
      title: "The Atlanta Constitution",
      place_id: "P00001",
      place: "Atlanta",
      state: "GA",
      first_year: 1868,
      last_year: null,
      pages: 9000,
      languages: ["eng"],
    },
  ],
};

const noHits: HitsResponse = {
  index_version: "v1",
  place: null,
  title: null,
  total: 0,
  items: [],
  next_cursor: null,
} as unknown as HitsResponse;

function show(search: string) {
  window.history.replaceState(null, "", `/${search}`);
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <App />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: false,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
  api.meta.mockResolvedValue(meta);
  api.places.mockResolvedValue(places);
  api.titles.mockResolvedValue(titles);
  api.hits.mockResolvedValue(noHits);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.clearAllMocks();
  window.history.replaceState(null, "", "/");
});

describe("App under the Congress source (#229)", () => {
  it("lists no newspapers and says why the relative rate is off", async () => {
    api.aggregate.mockResolvedValue(aggregate("PC001", 3, ""));
    show("?q=gold&source=congress");
    const rate = await screen.findByRole("button", { name: "Relative rate" });
    expect(rate.hasAttribute("disabled")).toBe(true);
    expect(rate.getAttribute("title")).toBe("Not for Congress pages yet: they have no pages-published counts.");
    // The pages are listed at Congress's place, with no newspaper table under them.
    expect(screen.getByRole("rowheader", { name: "Congress" })).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Newspapers" })).toBeNull();
  });

  it("lists the newspapers of a newspaper search, with the relative rate on, as before", async () => {
    api.aggregate.mockResolvedValue(aggregate("P00001", 3, "sn82014424"));
    show("?q=gold");
    const rate = await screen.findByRole("button", { name: "Relative rate" });
    expect(rate.hasAttribute("disabled")).toBe(false);
    expect(screen.getByRole("region", { name: "Newspapers" })).toBeTruthy();
  });

  it("doesn't suggest removing filters a Congress search doesn't have", async () => {
    api.aggregate.mockResolvedValue(aggregate("PC001", 0, ""));
    show("?q=gold&source=congress");
    const none = await screen.findByText(/No pages match/);
    expect(none.closest(".notice")?.textContent).toBe(
      "No pages match. Try “All words” instead of an exact phrase, or widen the dates.",
    );
  });

  it("still suggests removing a newspaper search's filters", async () => {
    api.aggregate.mockResolvedValue(aggregate("P00001", 0, ""));
    show("?q=gold&lang=ger");
    const none = await screen.findByText(/No pages match/);
    expect(none.closest(".notice")?.textContent).toBe(
      "No pages match. Try “All words” instead of an exact phrase, widen the dates, or remove language or state filters.",
    );
  });
});

describe("App under a newspaper filter (07 §7.13)", () => {
  it("names the papers from the newspapers' list and says why the relative rate is off", async () => {
    api.aggregate.mockResolvedValue(aggregate("P00001", 3, "sn82014424"));
    show("?q=gold&lccn=sn82014424,sn89053729");
    const note = await screen.findByText(/Only pages from/);
    await screen.findByText("The Atlanta Constitution", { selector: "cite" });
    expect(note.textContent).toBe(
      "Only pages from The Atlanta Intelligencer and The Atlanta Constitution. Show every newspaper",
    );
    expect(api.titles).toHaveBeenCalledWith("v1", expect.anything());
    const rate = screen.getByRole("button", { name: "Relative rate" });
    expect(rate.hasAttribute("disabled")).toBe(true);
    expect(rate.getAttribute("title")).toBe("Not for chosen newspapers: pages published aren't counted per newspaper.");
  });

  it("says the map shows one paper's place only", async () => {
    api.aggregate.mockResolvedValue(aggregate("P00001", 3, "sn82014424"));
    show("?q=gold&lccn=sn82014424");
    const note = await screen.findByText(/Only pages from/);
    expect(note.textContent).toBe(
      "Only pages from The Atlanta Intelligencer, so the map shows its place only. Show every newspaper",
    );
    expect(screen.getByRole("button", { name: "Relative rate" }).getAttribute("title")).toBe(
      "Not for one newspaper: pages published aren't counted per newspaper.",
    );
  });

  it("suggests removing the newspaper filter when nothing matches", async () => {
    api.aggregate.mockResolvedValue(aggregate("P00001", 0, ""));
    show("?q=gold&lccn=sn82014424");
    const none = await screen.findByText(/No pages match/);
    expect(none.closest(".notice")?.textContent).toBe(
      "No pages match. Try “All words” instead of an exact phrase, widen the dates, or remove newspaper, language or state filters.",
    );
  });

  it("doesn't load the newspapers' list until it is wanted", async () => {
    api.aggregate.mockResolvedValue(aggregate("P00001", 3, "sn82014424"));
    show("?q=gold");
    await screen.findByRole("region", { name: "Newspapers" });
    expect(api.titles).not.toHaveBeenCalled();
  });
});

describe("App's Median date lists (#264)", () => {
  // Forty places, every one's pages in 1850: too many tied in one bucket to
  // ask for their days at 10+5 (a budget of 30), but not at 15+5 (40).
  const ids = Array.from({ length: 40 }, (_, i) => `P${String(i).padStart(5, "0")}`);
  const name = (i: number) => `Town ${String(i).padStart(2, "0")}`;
  const tied: PlacesResponse = {
    type: "FeatureCollection",
    index_version: "v1",
    features: ids.map((id, i) => ({
      type: "Feature",
      id,
      geometry: { type: "Point", coordinates: [-90 + i * 0.1, 40] },
      properties: { name: name(i), state: "IL", precision: "city", titles: 1 },
    })),
  };
  const year: AggregateResponse = {
    ...aggregate("P00001", 10, "sn82014424"),
    total: { hits: 400, places: 40, baseline_pages: 0, papers: 1 },
    series: { hits: [400], baseline: null },
    places: { id: ids, hits: ids.map(() => 10), first_day: ids.map(() => dayNumber("1850-01-01")) },
    cube: { p: ids.map((_, i) => i), b: ids.map(() => 0), h: ids.map(() => 10), calls: 1, baseline_ref: null },
  };
  // By day, the higher a place's number the earlier its median: Town 39 first.
  const answer = (want: string[]): DaysResponse => ({
    index_version: "v1",
    places: want.map((id) => ({ id, days: [dayNumber("1850-01-01") + (39 - Number(id.slice(1))) * 5], hits: [10] })),
  });

  it("keeps the bucket-ranked lists, and focus on Show more, while a longer list's first days load", async () => {
    api.places.mockResolvedValue(tied);
    api.aggregate.mockResolvedValue(year);
    // The days wait for `release` (two requests: at most 20 places each).
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => (release = resolve));
    api.days.mockImplementation(async (_params: unknown, _version: unknown, want: string[]) => {
      await gate;
      return answer(want);
    });
    show("?q=gold&norm=when");
    const heading = await screen.findByRole("heading", { name: "Earliest median date" });
    const list = heading.closest("section")!;
    const rows = () => within(list).getAllByRole("listitem");
    const more = () => within(list).getByRole("button", { name: "Show more" });
    // 10+5: still too many tied places to ask for, ranked by bucket.
    fireEvent.click(more());
    expect(rows()).toHaveLength(10);
    expect(api.days).not.toHaveBeenCalled();
    // 15+5: their days are asked for. The lists stay, marked as ranking, with focus on Show more.
    const button = more();
    button.focus();
    fireEvent.click(button);
    await waitFor(() => expect(api.days).toHaveBeenCalled());
    expect(screen.queryByText("Finding each place's median day…")).toBeNull();
    expect(button.isConnected).toBe(true);
    expect(rows()).toHaveLength(15);
    expect(document.activeElement).toBe(button);
    await act(async () => release());
    // Ranked by day, Town 39 down, and focus on the first added in that order: Town 29.
    await waitFor(() => expect(rows()[0]!.textContent).toContain(name(39)));
    expect(rows()[10]!.textContent).toContain(name(29));
    expect(document.activeElement?.textContent).toBe(`${name(29)}, IL`);
  });

  it("lists nothing until the first exact answer when playback makes the first place eligible", async () => {
    // Two years: Atlanta's 2 pages in 1850 are too few for a median, its 12 by 1851 enough.
    const twoYears: AggregateResponse = {
      ...aggregate("P00001", 12, "sn82014424"),
      bucket: { unit: "year", from: "1850-01-01", to: "1851-12-31", count: 2 },
      series: { hits: [2, 10], baseline: null },
      cube: { p: [0, 0], b: [0, 1], h: [2, 10], calls: 1, baseline_ref: null },
    };
    api.aggregate.mockResolvedValue(twoYears);
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => (release = resolve));
    api.days.mockImplementation(async (_params: unknown, _version: unknown, want: string[]): Promise<DaysResponse> => {
      await gate;
      return { index_version: "v1", places: want.map((id) => ({ id, days: [dayNumber("1851-06-01")], hits: [12] })) };
    });
    show("?q=gold&norm=when&t=1850-01-01");
    // In 1850 no place can be listed, and no days are asked for.
    const list = (await screen.findByRole("heading", { name: "Earliest median date" })).closest("section")!;
    expect(within(list).queryAllByRole("listitem")).toHaveLength(0);
    expect(api.days).not.toHaveBeenCalled();
    // Playback reaches 1851: Atlanta's days are asked for, and nothing is listed until they come.
    fireEvent.change(screen.getByRole("slider", { name: "Time" }), { target: { value: "1" } });
    await waitFor(() => expect(api.days).toHaveBeenCalled());
    expect(screen.getByText("Finding each place's median day…")).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "Earliest median date" })).toBeNull();
    await act(async () => release());
    const ranked = (await screen.findByRole("heading", { name: "Earliest median date" })).closest("section")!;
    expect(within(ranked).getAllByRole("listitem")[0]!.textContent).toContain("Atlanta");
  });
});
