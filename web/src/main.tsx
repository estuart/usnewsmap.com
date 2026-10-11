import { lazy, StrictMode, Suspense } from "react";
import { createRoot } from "react-dom/client";
import { QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { isRetryable, VersionChangedError } from "./api/client";
import { NotFound } from "./components/NotFound";
import { trackPageView } from "./pageview";
import { DATASETS_PATH, OLD_DATASETS_PATH, pageFor } from "./route";
import "./styles.css";

// The dataset details and privacy pages are their own routes, loaded only
// when visited. The API serves index.html for app paths, so /datasets and
// /privacy work as direct links; other paths get it with a 404, and the
// not-found page.
const DatasetsPage = lazy(() => import("./status/StatusPage"));
const PrivacyPage = lazy(() => import("./privacy/PrivacyPage"));
const page = pageFor(window.location.pathname);
// The old path, where a server didn't redirect it: show the new one.
if (page === "datasets" && window.location.pathname.replace(/\/+$/, "") === OLD_DATASETS_PATH) {
  window.history.replaceState(null, "", `${DATASETS_PATH}${window.location.search}${window.location.hash}`);
}
// One page view per page shown; each page is a page load (no client router).
trackPageView(page);

const client: QueryClient = new QueryClient({
  // A new index version was published mid-session: refetch /v1/meta. Every
  // version-scoped query key includes the version, so they all move to the
  // new snapshot together instead of mixing old and new responses.
  queryCache: new QueryCache({
    onError: (err) => {
      if (err instanceof VersionChangedError) void client.invalidateQueries({ queryKey: ["meta"] });
    },
  }),
  defaultOptions: {
    queries: {
      staleTime: 5 * 60_000,
      // Problems (4xx), version changes, and searches that ran out of
      // time or found the API busy after waiting are not retried.
      retry: (n, err) => isRetryable(err) && n < 2,
      refetchOnWindowFocus: false,
    },
  },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={client}>
      {page === "datasets" ? (
        <Suspense fallback={<p role="status">Loading…</p>}>
          <DatasetsPage />
        </Suspense>
      ) : page === "privacy" ? (
        <Suspense fallback={<p role="status">Loading…</p>}>
          <PrivacyPage />
        </Suspense>
      ) : page === "not-found" ? (
        <NotFound />
      ) : (
        <App />
      )}
    </QueryClientProvider>
  </StrictMode>,
);
