import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { AggregateResponse, HitsResponse, Meta, PlacesResponse } from "./api/types";

// The API answers from these, so App can be shown for a search (no WebGL in
// jsdom: it shows the tables, not the map).
const { api } = vi.hoisted(() => ({
  api: { meta: vi.fn(), places: vi.fn(), aggregate: vi.fn(), coverage: vi.fn(), days: vi.fn(), hits: vi.fn() },
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
