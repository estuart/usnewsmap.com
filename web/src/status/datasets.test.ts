import { describe, expect, it } from "vitest";
import type { VersionItem } from "../api/types";
import { byDecade, featureChanges, pagesChange, publishedVersions, reasonLabel } from "./datasets";

const version = (v: Partial<VersionItem>): VersionItem => ({
  index_version: "pages-v1",
  serving: false,
  full: false,
  status: "published",
  indexes: [],
  new_index: "pages-v1",
  batches: null,
  docs: 0,
  pages: 0,
  duplicate_pages: 0,
  started_at: "2026-10-01T00:00:00Z",
  published_at: "2026-10-01T01:00:00Z",
  previous_version: null,
  build: null,
  build_source: null,
  ...v,
});

describe("byDecade", () => {
  it("sums years by decade, keeping empty decades between the first and last", () => {
    expect(
      byDecade([
        { year: 1789, pages: 3 },
        { year: 1801, pages: 5 },
        { year: 1809, pages: 7 },
        { year: 1820, pages: 1 },
      ]),
    ).toEqual([
      { decade: 1780, pages: 3 },
      { decade: 1790, pages: 0 },
      { decade: 1800, pages: 12 },
      { decade: 1810, pages: 0 },
      { decade: 1820, pages: 1 },
    ]);
    expect(byDecade([])).toEqual([]);
  });
});

describe("reasonLabel", () => {
  it("says why we read a page in words, and passes an unknown reason through", () => {
    expect(reasonLabel("missing")).toBe("No text from the Library of Congress");
    expect(reasonLabel("garbled")).toBe("The Library of Congress text is garbled");
    expect(reasonLabel("new_reason")).toBe("new_reason");
  });
});

describe("featureChanges", () => {
  const before = version({ build: { features: { common_grams: 1, ja_fold: 1, decades: 1, ja_latin: null } } });
  const features = { common_grams: 2, ja_fold: 1, american_stories: 1, ja_latin: 1, mystery: 4 };

  it("names features a full rebuild added, dropped and changed, against the version before", () => {
    expect(featureChanges(version({ full: true, build: { features } }), before)).toEqual([
      "Added American Stories' text",
      "New version of word pairs for phrase searches (2)",
      "Dropped the index split by decade",
      "Added the Latin-script text of our Japanese OCR",
      "Added mystery",
    ]);
  });

  it("doesn't call a feature an update didn't write dropped, since the update keeps the earlier indexes", () => {
    expect(featureChanges(version({ full: false, build: { features } }), before)).toEqual([
      "Added American Stories' text",
      "New version of word pairs for phrase searches (2)",
      "Added the Latin-script text of our Japanese OCR",
      "Added mystery",
    ]);
  });

  it("says nothing without both build records", () => {
    const recorded = version({ build: { features: { american_stories: 1 } } });
    expect(featureChanges(recorded, undefined)).toEqual([]);
    expect(featureChanges(recorded, version({}))).toEqual([]);
  });
});

describe("publishedVersions and pagesChange", () => {
  it("keeps published versions, newest first, each with the version it replaced", () => {
    const a = version({ index_version: "a", pages: 100, published_at: "2026-10-01T00:00:00Z" });
    const b = version({ index_version: "b", status: "failed", published_at: null, previous_version: "a" });
    const c = version({ index_version: "c", pages: 90, published_at: "2026-10-03T00:00:00Z", previous_version: "d" });
    const d = version({ index_version: "d", pages: 1100, published_at: "2026-10-02T00:00:00Z", previous_version: "a" });
    const list = publishedVersions([a, b, c, d]);
    expect(list.map(({ v, before }) => [v.index_version, before?.index_version])).toEqual([
      ["c", "d"],
      ["d", "a"],
      ["a", undefined],
    ]);
    expect(pagesChange(c, d)).toBe("−1,010");
    expect(pagesChange(d, a)).toBe("+1,000");
    expect(pagesChange(a, a)).toBe("no change");
    expect(pagesChange(a, undefined)).toBe("–");
  });

  it("pairs a version with the one it replaced after a rollback, or none when that one isn't listed", () => {
    // d went live after a rollback from c to a, so it replaced a, not c.
    const a = version({ index_version: "a", published_at: "2026-10-01T00:00:00Z", previous_version: "gone" });
    const c = version({ index_version: "c", published_at: "2026-10-02T00:00:00Z", previous_version: "a" });
    const d = version({ index_version: "d", published_at: "2026-10-03T00:00:00Z", previous_version: "a" });
    expect(publishedVersions([a, c, d]).map(({ v, before }) => [v.index_version, before?.index_version])).toEqual([
      ["d", "a"],
      ["c", "a"],
      ["a", undefined],
    ]);
  });
});
