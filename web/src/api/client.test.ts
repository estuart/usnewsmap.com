import { afterEach, describe, expect, it, vi } from "vitest";
import { api, ApiError, isPlain, isRetryable, MAX_COMPUTE_WAIT_MS, searchQuery, VersionChangedError } from "./client";

function respond(body: unknown, status = 200, type = "application/json") {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(JSON.stringify(body), { status, headers: { "content-type": type } })),
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

const computing = () =>
  new Response(JSON.stringify({ status: "computing", retry_after: 2 }), {
    status: 202,
    headers: { "content-type": "application/json", "retry-after": "2" },
  });
const problem = (status: number, type: string, retry?: string) =>
  new Response(JSON.stringify({ type, title: "t", status }), {
    status,
    headers: { "content-type": "application/problem+json", ...(retry ? { "retry-after": retry } : {}) },
  });
const ok = () =>
  new Response(JSON.stringify({ index_version: "v1", total: { hits: 1 } }), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

/** Answer each fetch with the next response in `seq`. */
function sequence(...seq: (() => Response)[]) {
  const fn = vi.fn(async () => seq.shift()!());
  vi.stubGlobal("fetch", fn);
  return fn;
}

describe("searches the API is still computing", () => {
  it("asks again after Retry-After until the result arrives", async () => {
    vi.useFakeTimers();
    const fetch = sequence(computing, computing, ok);
    const onComputing = vi.fn();
    const result = api.aggregate({ q: "radio" }, "v1", undefined, onComputing);
    await vi.advanceTimersByTimeAsync(1999);
    expect(fetch).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(fetch).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(2000);
    await expect(result).resolves.toMatchObject({ index_version: "v1" });
    expect(fetch).toHaveBeenCalledTimes(3);
    expect(onComputing).toHaveBeenCalledTimes(2);
    // The same URL each time.
    const urls = fetch.mock.calls.map((c) => String((c as unknown[])[0]));
    expect(new Set(urls).size).toBe(1);
  });

  it("reports the place in the API's queue while the search waits for a slot", async () => {
    vi.useFakeTimers();
    const queued = (ahead: number) => () =>
      new Response(JSON.stringify({ status: "queued", ahead, retry_after: 2 }), {
        status: 202,
        headers: { "content-type": "application/json", "retry-after": "2" },
      });
    sequence(queued(2), queued(0), computing, () => problem(503, "/errors/busy", "5"), ok);
    const onComputing = vi.fn();
    const result = api.aggregate({ q: "radio" }, "v1", undefined, onComputing);
    await vi.advanceTimersByTimeAsync(2000 * 3 + 5000);
    await expect(result).resolves.toMatchObject({ index_version: "v1" });
    expect(onComputing.mock.calls).toEqual([[2], [0], [null], [null]]);
  });

  it("stops asking when the search changes (the signal aborts)", async () => {
    vi.useFakeTimers();
    const fetch = sequence(computing, computing, ok);
    const ctl = new AbortController();
    const result = api.aggregate({ q: "radio" }, "v1", ctl.signal).catch((e: unknown) => e);
    await vi.advanceTimersByTimeAsync(500);
    ctl.abort(new DOMException("changed", "AbortError"));
    expect(((await result) as DOMException).name).toBe("AbortError");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("gives up after the longest wait, and that isn't retried", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(async () => computing());
    vi.stubGlobal("fetch", fetch);
    const result = api.aggregate({ q: "radio" }, "v1").catch((e: unknown) => e);
    await vi.advanceTimersByTimeAsync(MAX_COMPUTE_WAIT_MS + 5000);
    const err = (await result) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err.problem.type).toBe("/errors/backend-timeout");
    expect(fetch.mock.calls.length).toBeLessThanOrEqual(MAX_COMPUTE_WAIT_MS / 2000 + 1);
    expect(isRetryable(err)).toBe(false);
  });

  it("holds a request made while waiting to the longest wait", async () => {
    vi.useFakeTimers();
    let calls = 0;
    // The first answer is a 202; the next request never gets an answer.
    vi.stubGlobal(
      "fetch",
      vi.fn((_: unknown, init?: RequestInit) => {
        calls += 1;
        if (calls === 1) return Promise.resolve(computing());
        return new Promise<Response>((_, reject) =>
          init?.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError"))),
        );
      }),
    );
    const result = api.aggregate({ q: "radio" }, "v1").catch((e: unknown) => e);
    await vi.advanceTimersByTimeAsync(MAX_COMPUTE_WAIT_MS + 1000);
    const err = (await result) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err.problem.type).toBe("/errors/backend-timeout");
  });

  it("waits out a busy API, and a rate limit met while waiting", async () => {
    vi.useFakeTimers();
    const fetch = sequence(
      () => problem(503, "/errors/busy", "5"),
      computing,
      () => problem(429, "/errors/rate-limited", "3"),
      ok,
    );
    const result = api.aggregate({ q: "radio" }, "v1");
    await vi.advanceTimersByTimeAsync(5000 + 2000 + 3000);
    await expect(result).resolves.toMatchObject({ index_version: "v1" });
    expect(fetch).toHaveBeenCalledTimes(4);
  });

  it("a prefetch polls a 202 but gives up at once on a busy API or a rate limit", async () => {
    vi.useFakeTimers();
    for (const answer of [() => problem(503, "/errors/busy", "5"), () => problem(429, "/errors/rate-limited", "3")]) {
      const fetch = sequence(computing, answer, ok);
      const result = api
        .hits({ q: "radio" }, "v1", "P1", "oldest", null, undefined, () => true)
        .catch((e: unknown) => e);
      await vi.advanceTimersByTimeAsync(2000);
      const err = (await result) as ApiError;
      expect(err).toBeInstanceOf(ApiError);
      expect(fetch).toHaveBeenCalledTimes(2);
    }
  });

  it("a prefetch a visitor is now waiting on waits out a busy API like any request", async () => {
    vi.useFakeTimers();
    const fetch = sequence(() => problem(503, "/errors/busy", "5"), ok);
    const result = api.hits({ q: "radio" }, "v1", "P1", "oldest", null, undefined, () => false);
    await vi.advanceTimersByTimeAsync(5000);
    await expect(result).resolves.toMatchObject({ index_version: "v1" });
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("a rate limit before any 202 is an error, as before", async () => {
    sequence(() => problem(429, "/errors/rate-limited", "3"));
    await expect(api.aggregate({ q: "radio" }, "v1")).rejects.toBeInstanceOf(ApiError);
  });

  it("retries only failures worth retrying", () => {
    const err = (status: number, type: string) => new ApiError({ type, title: "t", status });
    expect(isRetryable(err(503, "/errors/backend"))).toBe(true);
    expect(isRetryable(new TypeError("network"))).toBe(true);
    expect(isRetryable(err(503, "/errors/backend-timeout"))).toBe(false);
    expect(isRetryable(err(503, "/errors/busy"))).toBe(false);
    expect(isRetryable(err(400, "/errors/query-syntax"))).toBe(false);
    expect(isRetryable(new VersionChangedError("a", "b"))).toBe(false);
  });
});

describe("api client", () => {
  it("rejects a response from another index version (a followed 307)", async () => {
    respond({ index_version: "v2", features: [] });
    await expect(api.places("v1")).rejects.toBeInstanceOf(VersionChangedError);
    respond({ index_version: "v1", type: "FeatureCollection", features: [] });
    await expect(api.places("v1")).resolves.toMatchObject({ index_version: "v1" });
  });

  it("treats a refusal after a followed 307 as a version change (#229)", async () => {
    // fetch follows the API's redirect of a stale `v`; the serving version refuses.
    const refusedAt = (url: string) => () =>
      Object.defineProperties(problem(422, "/errors/unsupported"), {
        redirected: { value: true },
        url: { value: url },
      });
    sequence(refusedAt("https://api.test/v1/aggregate?q=gold&source=congress&v=v2"));
    const err = await api.aggregate({ q: "gold", source: "congress" }, "v1").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(VersionChangedError);
    expect((err as VersionChangedError).actual).toBe("v2");
    // A refusal at the pinned version, or not redirected, is the API's answer.
    sequence(refusedAt("https://api.test/v1/aggregate?q=gold&source=congress&v=v1"));
    const same = await api.aggregate({ q: "gold", source: "congress" }, "v1").catch((e: unknown) => e);
    expect(same).toBeInstanceOf(ApiError);
    sequence(() => problem(422, "/errors/unsupported"));
    const direct = await api.days({ q: "gold", source: "congress" }, "v1", ["PC001"]).catch((e: unknown) => e);
    expect(direct).toBeInstanceOf(ApiError);
    expect((direct as ApiError).version).toBeNull();
  });

  it("keeps a timeout or rate limit after a followed 307 as the API's answer", async () => {
    // Already waited out: turning it into a version change would run the search again.
    const at = (resp: Response) =>
      Object.defineProperties(resp, {
        redirected: { value: true },
        url: { value: "https://api.test/v1/aggregate?q=gold&v=v2" },
      });
    for (const failed of [problem(503, "/errors/backend-timeout"), problem(429, "/errors/rate-limited")]) {
      sequence(() => at(failed));
      const err = await api.aggregate({ q: "gold" }, "v1").catch((e: unknown) => e);
      expect(err).toBeInstanceOf(ApiError);
      expect(err).not.toBeInstanceOf(VersionChangedError);
      expect((err as ApiError).version).toBe("v2");
    }
  });

  it("turns problem+json into ApiError", async () => {
    respond(
      { type: "/errors/query-syntax", title: "Query syntax error", status: 400, hint: "h" },
      400,
      "application/problem+json",
    );
    const err = await api.aggregate({ q: '"x' }, "v1").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).problem.hint).toBe("h");
  });

  it("revalidates /v1/meta instead of trusting the HTTP cache", async () => {
    respond({ index_version: "v2" });
    await api.meta();
    expect(vi.mocked(fetch).mock.calls[0]![1]).toMatchObject({ cache: "no-cache" });
  });

  it("pins every search to the version and omits defaults", async () => {
    respond({ index_version: "v1" });
    await api.aggregate({ q: "a b", mode: "all", bucket: "auto", state: ["GA"] }, "v1");
    const url = String(vi.mocked(fetch).mock.calls[0]![0]);
    expect(url).toBe("/v1/aggregate?q=a+b&state=GA&v=v1");
  });

  it("sends the language filter to searches and page lists", async () => {
    respond({ index_version: "v1" });
    await api.aggregate({ q: "gold", mode: "all", lang: ["ger", "spa"] }, "v1");
    expect(String(vi.mocked(fetch).mock.calls[0]![0])).toBe("/v1/aggregate?q=gold&lang=ger%2Cspa&v=v1");
    respond({ index_version: "v1" });
    await api.hits({ q: "gold", mode: "all", lang: ["ger"] }, "v1", "P00006", "oldest", null);
    expect(String(vi.mocked(fetch).mock.calls[0]![0])).toBe("/v1/hits?q=gold&lang=ger&v=v1&place=P00006&limit=20");
    respond({ index_version: "v1" });
    await api.hits({ q: "gold", mode: "all" }, "v1", "P00006", "newest", "abc");
    expect(String(vi.mocked(fetch).mock.calls[0]![0])).toBe(
      "/v1/hits?q=gold&v=v1&place=P00006&limit=20&sort=newest&cursor=abc",
    );
    expect(searchQuery({ q: "gold", mode: "all", lang: [] }, "v").toString()).toBe("q=gold&v=v");
  });

  it("asks for a newspaper's pages without a search word (#274)", async () => {
    respond({ index_version: "v1" });
    await api.titlePages("sn82014424", "v1", "oldest", null);
    expect(String(vi.mocked(fetch).mock.calls[0]![0])).toBe("/v1/hits?lccn=sn82014424&limit=20&v=v1");
    respond({ index_version: "v1" });
    await api.titlePages("sn82014424", "v1", "newest", "abc");
    expect(String(vi.mocked(fetch).mock.calls[0]![0])).toBe(
      "/v1/hits?lccn=sn82014424&limit=20&sort=newest&cursor=abc&v=v1",
    );
  });

  it("sends the mode for plain words and query syntax as-is", () => {
    const q = (p: Parameters<typeof searchQuery>[0]) => searchQuery(p, "v").toString();
    expect(q({ q: "cross of gold", mode: "phrase" })).toBe("q=cross+of+gold&mode=phrase&v=v");
    expect(q({ q: "cross of gold" })).toBe("q=cross+of+gold&mode=phrase&v=v");
    expect(q({ q: "yellow fever", mode: "near", near: 5 })).toBe("q=yellow+fever&mode=near&near=5&v=v");
    expect(q({ q: "free silver", mode: "any" })).toBe("q=free+silver&mode=any&v=v");
    expect(q({ q: '"cross of gold"', mode: "phrase" })).toBe("q=%22cross+of+gold%22&v=v");
    expect(q({ q: "fever OR influenza", mode: "phrase" })).toBe("q=fever+OR+influenza&v=v");
    expect(isPlain("gold -silver")).toBe(false);
    expect(isPlain("gold-standard")).toBe(true);
    expect(isPlain("pres?dent")).toBe(false);
    expect(isPlain("who was president?")).toBe(true);
    expect(q({ q: "pres?dent lincoln", mode: "phrase" })).toBe("q=pres%3Fdent+lincoln&v=v");
  });
});

describe("searchQuery and the Congress source (#229)", () => {
  it("sends a source only for Congress, with its series", () => {
    const q = (p: Parameters<typeof searchQuery>[0]) => searchQuery(p, "v").toString();
    expect(q({ q: "gold", mode: "all", source: "news", series: ["globe"] })).toBe("q=gold&v=v");
    expect(q({ q: "gold", mode: "all", source: "congress" })).toBe("q=gold&source=congress&v=v");
    expect(q({ q: "gold", mode: "all", source: "congress", series: ["globe", "serialset"] })).toBe(
      "q=gold&source=congress&series=globe%2Cserialset&v=v",
    );
  });
});
