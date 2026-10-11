import { describe, expect, it } from "vitest";
import { DEFAULTS, isIsoDate, parseView, searchParams, serializeView } from "./url";

describe("view URL", () => {
  it("round-trips and writes only non-defaults", () => {
    const v = parseView(
      "?q=%22cross+of+gold%22&from=1896-06-01&to=1896-12-31&bucket=week&t=1896-07-12&win=4&layer=heat&norm=skew&state=il,ny&lang=ger&place=P00412&sort=newest&z=4.2&c=-92.1,39.4",
    );
    expect(v.q).toBe('"cross of gold"');
    expect(v.state).toEqual(["IL", "NY"]);
    expect(v.lang).toEqual(["ger"]);
    expect(v.win).toBe(4);
    expect(v.c).toEqual([-92.1, 39.4]);
    expect(v.norm).toBe("skew");
    expect(v.sort).toBe("newest");
    expect(parseView(serializeView(v))).toEqual(v);
    expect(serializeView(DEFAULTS)).toBe("");
    expect(serializeView({ ...DEFAULTS, q: "fever" })).toBe("?q=fever");
    expect(parseView("?sort=relevant").sort).toBe("relevant");
    expect(parseView("?norm=when").norm).toBe("when");
    // No relative rate for one newspaper, even from a shared link.
    const shared = parseView("?q=gold&lccn=sn99000001&norm=skew");
    expect(shared.norm).toBe("raw");
    expect(serializeView({ ...shared, norm: "skew" })).toBe("?q=gold&lccn=sn99000001");
    // The newspaper filter (#121): lowercased, sorted, invalid codes dropped.
    const paper = parseView("?q=gold&lccn=SN99000002,sn99000001,bad!code");
    expect(paper.lccn).toEqual(["sn99000001", "sn99000002"]);
    expect(serializeView(paper)).toBe("?q=gold&lccn=sn99000001%2Csn99000002");
    expect(searchParams(paper).lccn).toEqual(["sn99000001", "sn99000002"]);
    // As many as the API takes (60), so a valid permalink keeps its search.
    const many = Array.from({ length: 60 }, (_, i) => `sn${String(i).padStart(8, "0")}`);
    expect(parseView(`?lccn=${many.join(",")}`).lccn).toEqual(many);
  });

  it("drops invalid values instead of trusting them", () => {
    const v = parseView(
      "?mode=evil&bucket=hour&from=1896-6-1&win=-3&layer=3d&z=99&c=500,1&place=%3Cscript%3E&state=Illinois&lang=%3Cb%3E&sort=random",
    );
    expect(v).toEqual(DEFAULTS);
  });

  it("accepts only real calendar dates", () => {
    expect(isIsoDate("1896-02-29")).toBe(true);
    expect(isIsoDate("1897-02-29")).toBe(false);
    expect(isIsoDate("1896-02-31")).toBe(false);
    expect(isIsoDate("1896-13-01")).toBe(false);
    expect(parseView("?from=1896-02-31&to=1896-04-31&t=1896-06-31")).toEqual(DEFAULTS);
  });

  it("rejects blank coordinates and zooms the map can't show", () => {
    for (const q of ["?c=,", "?c=12,", "?c=,40", "?z=20", "?z=1", "?z="]) {
      const v = parseView(q);
      expect([v.c, v.z], q).toEqual([null, null]);
    }
    expect(parseView("?z=12&c=0,0")).toMatchObject({ z: 12, c: [0, 0] });
  });

  it("keeps the measure in the URL; the removed share of pages opens on Pages", () => {
    expect(parseView("?q=fever&norm=skew").norm).toBe("skew");
    expect(serializeView({ ...DEFAULTS, q: "fever", norm: "skew" })).toBe("?q=fever&norm=skew");
    // Old permalinks to the share of pages published fall back to Pages.
    expect(parseView("?q=fever&norm=rel").norm).toBe("raw");
    expect(serializeView(parseView("?q=fever&norm=rel"))).toBe("?q=fever");
    expect(parseView("?q=fever&norm=lift").norm).toBe("raw");
  });

  it("keeps the language filter in the API's canonical order", () => {
    const v = parseView("?q=fever&lang=SPA,ger,spa,%20eng");
    expect(v.lang).toEqual(["eng", "ger", "spa"]);
    expect(serializeView(v)).toBe("?q=fever&lang=eng%2Cger%2Cspa");
    expect(parseView(serializeView(v))).toEqual(v);
    // Unsorted or repeated codes set in code are written canonically too.
    expect(serializeView({ ...DEFAULTS, q: "fever", lang: ["spa", "ger", "spa"] })).toBe("?q=fever&lang=ger%2Cspa");
    expect(searchParams(v).lang).toEqual(["eng", "ger", "spa"]);
    // Anything but three letters is dropped; nothing left means no filter.
    expect(parseView("?q=fever&lang=german,e1g,,pennsylvania%20german").lang).toEqual([]);
    expect(serializeView(parseView("?q=fever&lang=xx"))).toBe("?q=fever");
    expect(parseView("?q=fever&lang=ger,english").lang).toEqual(["ger"]);
  });

  it("keeps near only in near mode", () => {
    expect(serializeView({ ...DEFAULTS, q: "a b", mode: "near", near: 8 })).toBe("?q=a+b&mode=near&near=8");
    expect(serializeView({ ...DEFAULTS, q: "a b", near: 8 })).toBe("?q=a+b");
  });
});

describe("Congress source (#229)", () => {
  it("leaves newspapers out of the URL and the search", () => {
    const v = parseView("?q=gold");
    expect(v.source).toBe("news");
    expect(serializeView(v)).toBe("?q=gold");
    expect(searchParams(v)).not.toHaveProperty("source");
    expect(parseView("?q=gold&source=news")).toEqual(v);
  });

  it("round-trips a Congress search with its series, sorted and known only", () => {
    const v = parseView("?q=gold&source=congress&series=serialset,globe,record,globe");
    expect(v.source).toBe("congress");
    expect(v.series).toEqual(["globe", "serialset"]);
    expect(serializeView(v)).toBe("?q=gold&source=congress&series=globe%2Cserialset");
    expect(searchParams(v)).toMatchObject({ source: "congress", series: ["globe", "serialset"], lccn: [] });
  });

  it("drops what a Congress search can't use", () => {
    // No newspapers to filter by, and no baselines for a relative rate yet.
    const v = parseView("?q=gold&source=congress&lccn=sn99000001&norm=skew");
    expect(v.lccn).toEqual([]);
    expect(v.norm).toBe("raw");
    // A series without the Congress source means nothing.
    expect(parseView("?q=gold&series=globe").series).toEqual([]);
    expect(parseView("?q=gold&source=senate").source).toBe("news");
  });

  it("drops the state and language filters, which a Congress search hides", () => {
    // From a shared link they would filter out every Congress page, unseen.
    const v = parseView("?q=gold&source=congress&state=GA&lang=ger");
    expect(v.state).toEqual([]);
    expect(v.lang).toEqual([]);
    expect(serializeView(v)).toBe("?q=gold&source=congress");
    // Set in code, too.
    const set = { ...v, state: ["GA"], lang: ["ger"] };
    expect(serializeView(set)).toBe("?q=gold&source=congress");
    expect(searchParams(set)).toMatchObject({ state: [], lang: [], lccn: [] });
  });

  it("keeps a newspaper search's parameters, keys and order as before", () => {
    const v = parseView("?q=gold&state=ga&lang=ger&lccn=sn99000001");
    expect(serializeView(v)).toBe("?q=gold&state=GA&lang=ger&lccn=sn99000001");
    // App keys its queries on this JSON, so the examples' cached answers stay valid.
    expect(JSON.stringify(searchParams(v))).toBe(
      '{"q":"gold","mode":"phrase","near":5,"from":"","to":"","bucket":"auto","state":["GA"],"lang":["ger"],"lccn":["sn99000001"]}',
    );
  });
});
