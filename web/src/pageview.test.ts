import { afterEach, describe, expect, it, vi } from "vitest";
import { enabled, MAX_UTM, optedOut, referrerOrigin, report, send, SITE_ORIGIN, utmTags } from "./pageview";

const landing = (referrer: string, search: string) => ({ referrer, search, origin: SITE_ORIGIN });

describe("enabled", () => {
  const base = { prod: true, flag: undefined, origin: SITE_ORIGIN, nav: {} };

  it("reports only from the production site unless the flag says otherwise", () => {
    expect(enabled(base)).toBe(true);
    expect(enabled({ ...base, prod: false })).toBe(false);
    expect(enabled({ ...base, origin: "http://127.0.0.1:4173" })).toBe(false);
    expect(enabled({ ...base, origin: "https://www.usnewsmap.com" })).toBe(false);
    expect(enabled({ ...base, prod: false, origin: "http://localhost:5173", flag: "1" })).toBe(true);
    expect(enabled({ ...base, flag: "0" })).toBe(false);
  });

  it("honors Do Not Track and Global Privacy Control, even with the flag", () => {
    expect(optedOut({ doNotTrack: "1" })).toBe(true);
    expect(optedOut({}, { doNotTrack: "1" })).toBe(true);
    expect(optedOut({ globalPrivacyControl: true })).toBe(true);
    expect(optedOut({ doNotTrack: "0", globalPrivacyControl: false })).toBe(false);
    expect(optedOut({ doNotTrack: null })).toBe(false);
    expect(enabled({ ...base, nav: { doNotTrack: "1" } })).toBe(false);
    expect(enabled({ ...base, flag: "1", nav: { globalPrivacyControl: true } })).toBe(false);
  });
});

describe("referrerOrigin", () => {
  it("keeps only the origin", () => {
    expect(referrerOrigin("https://www.google.com/search?q=cross+of+gold", SITE_ORIGIN)).toBe("https://www.google.com");
    expect(referrerOrigin("http://localhost:8080/a/b#c", SITE_ORIGIN)).toBe("http://localhost:8080");
    expect(referrerOrigin(`${SITE_ORIGIN}/?q=zebrasecret`, SITE_ORIGIN)).toBe("internal");
  });

  it("is empty for no referrer or a non-web one", () => {
    expect(referrerOrigin("", SITE_ORIGIN)).toBe("");
    expect(referrerOrigin("not a url", SITE_ORIGIN)).toBe("");
    expect(referrerOrigin("android-app://com.google.android.gm/", SITE_ORIGIN)).toBe("");
  });
});

describe("utmTags", () => {
  it("reads the three tags, lowercased and capped, and nothing else", () => {
    expect(
      utmTags(
        "?q=zebrasecret&utm_source=NewsLetter&utm_medium=Email&utm_campaign=Fall%202026&utm_term=gold&from=1896-01-01",
      ),
    ).toEqual({ utm_source: "newsletter", utm_medium: "email", utm_campaign: "fall 2026" });
    expect(utmTags("")).toEqual({});
    expect(utmTags("?utm_source=&utm_medium=%20")).toEqual({});
    expect(utmTags(`?utm_source=${"x".repeat(100)}`).utm_source).toHaveLength(MAX_UTM);
    expect(utmTags("?utm_source=a%0Ab%09c").utm_source).toBe("abc");
  });
});

describe("report", () => {
  it("carries the referrer origin and tags on the first page only, and never the search", () => {
    const first = report("search", true, landing("https://t.co/abc", "?q=zebrasecret&state=GA&utm_source=X"));
    expect(first).toEqual({
      route: "search",
      title: "US News Map",
      referrer_origin: "https://t.co",
      utm_source: "x",
    });
    expect(JSON.stringify(first)).not.toContain("zebrasecret");
    expect(JSON.stringify(first)).not.toContain("GA");
    expect(report("datasets", false, landing("https://t.co/abc", "?utm_source=x"))).toEqual({
      route: "datasets",
      title: "Dataset details · US News Map",
      referrer_origin: "internal",
    });
    expect(report("privacy", true, landing("", "")).referrer_origin).toBe("");
  });

  it("uses only the fields the API accepts", () => {
    const r = report("not-found", true, landing("https://example.com/x", "?utm_medium=a&utm_campaign=b&utm_source=c"));
    expect(Object.keys(r).sort()).toEqual(
      ["referrer_origin", "route", "title", "utm_campaign", "utm_medium", "utm_source"].sort(),
    );
  });
});

describe("send", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("uses sendBeacon with a text body", () => {
    const sendBeacon = vi.fn(() => true);
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);
    send({ route: "search", title: "US News Map", referrer_origin: "" }, { sendBeacon });
    expect(sendBeacon).toHaveBeenCalledWith(
      "/v1/beacon",
      JSON.stringify({ route: "search", title: "US News Map", referrer_origin: "" }),
    );
    expect(fetch).not.toHaveBeenCalled();
  });

  it("falls back to fetch with keepalive when sendBeacon refuses or is missing", () => {
    const fetch = vi.fn(() => Promise.resolve(new Response(null, { status: 204 })));
    vi.stubGlobal("fetch", fetch);
    const body = { route: "datasets" as const, title: "t", referrer_origin: "internal" };
    send(body, { sendBeacon: () => false });
    send(body, {} as Pick<Navigator, "sendBeacon">);
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(fetch).toHaveBeenCalledWith("/v1/beacon", {
      method: "POST",
      body: JSON.stringify(body),
      keepalive: true,
      credentials: "omit",
      headers: { "content-type": "application/json" },
    });
  });
});
