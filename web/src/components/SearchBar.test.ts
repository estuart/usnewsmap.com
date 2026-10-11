import { describe, expect, it } from "vitest";
import { DEFAULTS } from "../state/url";
import { activeOptions, searchBounds, searchKey } from "./SearchBar";

describe("activeOptions", () => {
  it("counts only options that differ from the defaults", () => {
    expect(activeOptions(DEFAULTS)).toBe(0);
    expect(activeOptions({ ...DEFAULTS, mode: "all" })).toBe(1);
    expect(activeOptions({ ...DEFAULTS, from: "1890-01-01", to: "1900-12-31", state: ["GA", "SC"] })).toBe(3);
    // Any number of languages is one option.
    expect(activeOptions({ ...DEFAULTS, lang: ["ger", "spa"] })).toBe(1);
  });
});

describe("searchKey", () => {
  it("changes with the language filter, so the form shows the URL's choice", () => {
    expect(searchKey({ ...DEFAULTS, q: "gold", lang: ["ger"] })).not.toBe(searchKey({ ...DEFAULTS, q: "gold" }));
    // The newspaper filter is part of the search too (#121).
    expect(searchKey({ ...DEFAULTS, q: "gold", lccn: ["sn99000001"] })).not.toBe(searchKey({ ...DEFAULTS, q: "gold" }));
  });
});

describe("Congress source (#229)", () => {
  it("counts the source as an option and keys the form on it", () => {
    expect(activeOptions({ ...DEFAULTS, source: "congress" })).toBe(1);
    // The state and language filters a Congress search hides don't count.
    expect(activeOptions({ ...DEFAULTS, source: "congress", state: ["GA"], lang: ["ger"] })).toBe(1);
    expect(searchKey({ ...DEFAULTS, q: "gold", source: "congress" })).not.toBe(searchKey({ ...DEFAULTS, q: "gold" }));
  });

  it("bounds a Congress search by its series' dates", () => {
    const meta = {
      bounds: { from: "1736-09-03", to: "1963-12-31" },
      congress: {
        series: [
          { series: "annals" as const, pages: 1, from: "1789-03-04", to: "1824-05-27" },
          { series: "serialset" as const, pages: 1, from: "1817-12-01", to: "1994-12-31" },
        ],
        place: { id: "PC001", name: "Congress", lat: 38.89, lon: -77.01 },
      },
    };
    expect(searchBounds(meta, "congress")).toEqual({ from: "1789-03-04", to: "1994-12-31" });
    expect(searchBounds(meta, "news")).toEqual({ from: "1736-09-03", to: "1963-12-31" });
    expect(searchBounds({ ...meta, congress: null }, "congress")).toEqual(meta.bounds);
  });
});
