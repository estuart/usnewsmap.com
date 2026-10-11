import { expect, test, type Page, type Route } from "@playwright/test";

// Page views (07 §7.8). Only the production site reports them, so these
// tests load the build as https://usnewsmap.com: every request to that
// origin is answered by the local server, and page views are captured
// instead of sent.

const SITE = "https://usnewsmap.com";

type Captured = { body: string; contentType: string };

// Requests still being proxied when a test finishes would fail with "route.fetch: Test ended".
test.afterEach(async ({ page }) => {
  await page.context().unrouteAll({ behavior: "ignoreErrors" });
});

/** Routes for every page of the context, so a second page is covered too. */
async function asProduction(page: Page, baseURL: string | undefined): Promise<Captured[]> {
  const reports: Captured[] = [];
  const context = page.context();
  await context.route("https://tiles.openfreemap.org/**", (route) => route.fulfill({ status: 404 }));
  await context.route(`${SITE}/**`, async (route: Route) => {
    const req = route.request();
    const url = new URL(req.url());
    if (url.pathname === "/v1/beacon") {
      reports.push({ body: req.postData() ?? "", contentType: req.headers()["content-type"] ?? "" });
      return route.fulfill({ status: 204 });
    }
    const response = await route.fetch({ url: new URL(url.pathname + url.search, baseURL).toString() });
    return route.fulfill({ response });
  });
  return reports;
}

test("the production site reports each page by name, never the search", async ({ page, baseURL }) => {
  const reports = await asProduction(page, baseURL);
  await page.goto(`${SITE}/?q=zebrasecret&state=GA&utm_source=Newsletter&utm_term=gold`, {
    referer: "https://www.google.com/search?q=zebrasecret",
  });
  await expect(page.getByRole("searchbox")).toHaveValue("zebrasecret");
  await expect.poll(() => reports.length).toBe(1);
  expect(JSON.parse(reports[0]!.body)).toEqual({
    route: "search",
    title: "US News Map",
    referrer_origin: "https://www.google.com",
    utm_source: "newsletter",
  });
  // sendBeacon's string body: text/plain, which needs no preflight.
  expect(reports[0]!.contentType).toMatch(/^text\/plain/);
  expect(reports[0]!.body).not.toContain("zebrasecret");
  expect(reports[0]!.body).not.toContain("GA");

  // Following a link on the site: an internal page view.
  await page.getByRole("link", { name: "Privacy" }).click();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Privacy");
  await expect.poll(() => reports.length).toBe(2);
  expect(JSON.parse(reports[1]!.body)).toEqual({
    route: "privacy",
    title: "Privacy · US News Map",
    referrer_origin: "internal",
  });
});

test("Global Privacy Control and Do Not Track turn page views off", async ({ page, baseURL }) => {
  const reports = await asProduction(page, baseURL);
  await page.addInitScript(() => {
    Object.defineProperty(Navigator.prototype, "globalPrivacyControl", { get: () => true });
  });
  await page.goto(`${SITE}/privacy`);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Privacy");

  const dnt = await page.context().newPage();
  await dnt.addInitScript(() => {
    Object.defineProperty(Navigator.prototype, "doNotTrack", { get: () => "1" });
  });
  await dnt.goto(`${SITE}/datasets`);
  await expect(dnt.getByRole("heading", { level: 1 })).toHaveText("Dataset details");
  await page.waitForTimeout(500);
  expect(reports).toEqual([]);

  // The same context without either signal does report.
  const plain = await page.context().newPage();
  await plain.goto(`${SITE}/datasets`);
  await expect.poll(() => reports.length).toBe(1);
  expect(JSON.parse(reports[0]!.body)).toMatchObject({ route: "datasets" });
});

test("other origins (development, these tests) send no page views", async ({ page }) => {
  const sent: string[] = [];
  page.on("request", (r) => {
    if (r.url().includes("/v1/beacon")) sent.push(r.url());
  });
  await page.route("https://tiles.openfreemap.org/**", (route) => route.fulfill({ status: 404 }));
  await page.goto("/privacy");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Privacy");
  await page.waitForTimeout(500);
  expect(sent).toEqual([]);
});

test("the site stores nothing in the browser", async ({ page, baseURL }) => {
  await asProduction(page, baseURL);
  await page.goto(`${SITE}/?q=%22cross+of+gold%22&from=1896-01-01&to=1897-12-31`);
  await expect(page.getByText("places ·")).toBeVisible();
  await page.waitForLoadState("networkidle");
  const stored = await page.evaluate(async () => ({
    cookies: document.cookie,
    local: localStorage.length,
    session: sessionStorage.length,
    databases: (await indexedDB.databases()).length,
    caches: (await caches.keys()).length,
  }));
  expect(stored).toEqual({ cookies: "", local: 0, session: 0, databases: 0, caches: 0 });
  expect(await page.context().cookies()).toEqual([]);
});
