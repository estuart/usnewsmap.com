import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";

// The newspaper filter (07 §7.13) against the API serving the synthetic
// fixtures: six titles, "The Fixture Gazette 1" to "6", one per place.

const STYLE = {
  version: 8,
  sources: {},
  layers: [{ id: "bg", type: "background", paint: { "background-color": "#e8e4dc" } }],
};

test.beforeEach(async ({ page }) => {
  await page.route("https://tiles.openfreemap.org/**", (route) =>
    route.request().url().includes("/styles/") ? route.fulfill({ json: STYLE }) : route.fulfill({ status: 404 }),
  );
});

async function expectAccessible(page: Page) {
  const results = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"])
    .exclude(".maplibregl-canvas-container")
    .exclude(".maplibregl-ctrl-attrib")
    .analyze();
  expect(results.violations.map((v) => `${v.id}: ${v.help}`)).toEqual([]);
}

/** On phones the filters sit behind "Options"; open it if it's shown. */
async function openOptions(page: Page) {
  const toggle = page.getByRole("button", { name: /^Options/ });
  if (await toggle.isVisible()) {
    if ((await toggle.getAttribute("aria-expanded")) !== "true") await toggle.click();
  }
}

test("choose newspapers by title or place, search within them, and reach their issues", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto("/?q=gold&from=1895-01-01&to=1897-12-31&tab=table");
  const papers = page.getByRole("region", { name: "Newspapers" });
  await expect(papers.locator("tbody tr")).toHaveCount(6);

  await openOptions(page);
  const button = page.getByRole("button", { name: "Newspapers: any" });
  await button.click();
  const box = page.getByRole("combobox", { name: "Newspapers" });
  await expect(box).toBeEnabled();

  // By title, from the keyboard: Enter chooses and doesn't search.
  await box.fill("gazette 2");
  const options = page.getByRole("listbox", { name: "Newspapers found" }).getByRole("option");
  await expect(options).toHaveCount(1);
  await expect(options.first()).toContainText("The Fixture Gazette 2");
  await expect(options.first()).toContainText("New York area");
  await expectAccessible(page);
  await box.press("ArrowDown");
  await box.press("Enter");
  await expect(page).not.toHaveURL(/lccn=/);
  // By place.
  await box.fill("nebraska");
  await expect(options).toHaveCount(1);
  await options.first().click();
  await expect(page.getByRole("button", { name: "Newspapers: 2 chosen" })).toBeVisible();
  const chosen = page.getByRole("list", { name: "Chosen newspapers" });
  await expect(chosen.getByRole("link", { name: "Issues of The Fixture Gazette 6 at loc.gov" })).toHaveAttribute(
    "href",
    "https://www.loc.gov/item/sn99000006/",
  );
  await expectAccessible(page);

  await page.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page).toHaveURL(/lccn=sn99000002(%2C|,)sn99000006/);
  await expect(page.getByRole("status").filter({ hasText: "Only pages from" })).toContainText(
    "Only pages from The Fixture Gazette 2 and The Fixture Gazette 6.",
  );
  await expect(papers.locator("tbody tr")).toHaveCount(2);
  await expect(papers.getByRole("link", { name: "Issues of The Fixture Gazette 2 at loc.gov" })).toBeVisible();

  // One paper: the notice says the map has its place only.
  await page.reload();
  await openOptions(page);
  await page.getByRole("button", { name: "Newspapers: 2 chosen" }).click();
  await page.getByRole("button", { name: "Remove The Fixture Gazette 6" }).click();
  await page.getByRole("button", { name: "Search", exact: true }).click();
  await expect(page).toHaveURL(/lccn=sn99000002(&|$)/);
  await expect(page.getByRole("status").filter({ hasText: "Only pages from" })).toContainText(
    "so the map shows its place only",
  );
  expect(errors).toEqual([]);
});
