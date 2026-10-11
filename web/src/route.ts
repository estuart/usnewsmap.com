/**
 * The page for a path. The app has no router: `/` is the search, `/datasets`
 * the dataset details, `/privacy` the privacy notice, and anything else not
 * found. The API answers the same paths with 200 and the rest with 404
 * (`is_app_page` in crates/usnm-api/src/site.rs). `/index.html` is the home
 * page's file, which the API serves with a 200, with or without a trailing
 * slash. `/status`, the dataset details' old path, is redirected to
 * `/datasets` by the API; the app shows the same page there too, for a
 * server that doesn't redirect (the dev server). The page names are also the
 * routes `/v1/beacon` accepts (`ROUTES` in crates/usnm-api/src/routes/beacon.rs).
 */
export type Page = "search" | "datasets" | "privacy" | "not-found";

/** The dataset details page's path, and its old one (#294). */
export const DATASETS_PATH = "/datasets";
export const OLD_DATASETS_PATH = "/status";

export function pageFor(pathname: string): Page {
  switch (pathname.replace(/\/+$/, "")) {
    case "":
    case "/index.html":
      return "search";
    case DATASETS_PATH:
    case OLD_DATASETS_PATH:
      return "datasets";
    case "/privacy":
      return "privacy";
    default:
      return "not-found";
  }
}

/** Each page's document title. None of them holds search text. */
export const TITLES: Record<Page, string> = {
  search: "US News Map",
  datasets: "Dataset details · US News Map",
  privacy: "Privacy · US News Map",
  "not-found": "Page not found · US News Map",
};
