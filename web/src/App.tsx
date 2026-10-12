import { Fragment, lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, ApiError, sameSearch, VersionChangedError, type SearchParams } from "./api/client";
import type { Problem } from "./api/types";
import { indexSummary } from "./lib/indexSummary";
import { alignCube, prefixSums, relative, windowQuantile, windowValues } from "./engine/cube";
import { EXAMPLE_ORDER } from "./examples";
import { Examples } from "./components/Examples";
import { bucketIndex, bucketLabel, bucketStart, dayNumber } from "./lib/time";
import { cssColor, cssTimeColor } from "./lib/scale";
import { searchParams, serializeView, useView, type ViewState } from "./state/url";
import { SearchBar, searchKey } from "./components/SearchBar";
import { Searching } from "./components/Searching";
import { Timeline } from "./components/Timeline";
import { TimeDock } from "./components/TimeDock";
import { PlacePanel } from "./components/PlacePanel";
import { PaperPages, type PaperSort } from "./components/PaperPages";
import { usePrefetchPlaceHits } from "./components/placeHits";
import { PlaceTable } from "./components/PlaceTable";
import { NewspaperTable, paperRows } from "./components/NewspaperTable";
import { paperFilterNames } from "./components/NewspaperFilter";
import { languageMix } from "./lib/languages";
import { Mentions } from "./components/Mentions";
import { About } from "./components/About";
import type { MapPoint } from "./components/mapTypes";
import { MeasureToggle } from "./components/MeasureToggle";
import { InfoTip } from "./components/InfoTip";
import { Tooltip } from "./components/Tooltip";
import { americanStoriesOnlyNote } from "./lib/matchSource";
import { baselineNote } from "./lib/baseline";
import { SkewLegend } from "./components/SkewLegend";
import { useMediaQuery } from "./lib/useMediaQuery";
import {
  PagesLists,
  WhenLists,
  daysFor,
  exactMedian,
  medianCandidates,
  type MedianStatus,
  type PlaceDays,
} from "./components/PlaceLists";
import { DownloadCsv, SkewLists, StateTable, clearest, type SkewRow } from "./components/SkewPanels";
import { useListLengths } from "./components/ListLength";
import { hasWebGL2 } from "./lib/webgl";
import { MIN_PLACES, prepareSkew, type Prepared, type Unavailable } from "./engine/skewInput";
import { maxWindowExpected, scoreFrame } from "./engine/skewModel";
import { useSkewModel } from "./engine/useSkew";
import { skewInfo } from "./lib/skewText";
import { Brand } from "./components/Brand";
import { congressFeature } from "./lib/congress";

const MapView = lazy(() => import("./components/MapView"));
const webgl = typeof document !== "undefined" && hasWebGL2();
/** The side panel lists that show more on request: Clearest differences (relative rate), Pages and Median date. */
const LIST_NAMES = ["above", "below", "places", "states", "earliest", "latest"] as const;

export function App() {
  const [view, setView] = useView();
  // How many times "Show more" was pressed under the home page's examples;
  // kept across searches so Back returns to the same cards.
  const [exampleClicks, setExampleClicks] = useState(0);
  const about = useRef<HTMLDialogElement>(null);
  // showModal() focuses the first link, which then shows a focus ring before anyone has tabbed.
  // Start on the heading instead: screen readers announce the dialog by it, and Tab goes on to
  // the first link.
  const openAbout = () => {
    about.current?.showModal();
    about.current?.querySelector<HTMLElement>("#about-title")?.focus();
  };
  const meta = useQuery({ queryKey: ["meta"], queryFn: ({ signal }) => api.meta(signal) });
  const version = meta.data?.index_version ?? "";
  const places = useQuery({
    queryKey: ["places", version],
    queryFn: ({ signal }) => api.places(version, signal),
    enabled: !!version,
    staleTime: Infinity,
  });
  // The newspapers' list (07 §7.13) loads once per version, when the
  // newspaper filter is first opened or the URL names a newspaper.
  const [wantTitles, setWantTitles] = useState(false);
  const titles = useQuery({
    queryKey: ["titles", version],
    queryFn: ({ signal }) => api.titles(version, signal),
    enabled: !!version && (wantTitles || view.lccn.length > 0),
    staleTime: Infinity,
  });
  const wantTitlesNow = useCallback(() => setWantTitles(true), []);
  // A newspaper's pages without a search word (07 §7.13, #274), opened from
  // the newspaper filter when the API lists them (`browse_pages`).
  // `returnFocus` puts focus back on what opened the list when it closes.
  const [browsing, setBrowsing] = useState<{
    lccn: string;
    sort: PaperSort;
    returnFocus: () => void;
  } | null>(null);
  const browsePaper = useCallback(
    (lccn: string, returnFocus: () => void) => setBrowsing({ lccn, sort: "oldest", returnFocus }),
    [],
  );

  // Parsed afresh on every URL change; a stable key keeps queries cached.
  const paramsKey = JSON.stringify(searchParams(view));
  const params = useMemo(() => JSON.parse(paramsKey) as SearchParams, [paramsKey]);

  // The search the API said it is still computing (a `202`), by query key,
  // and how many searches are ahead of it while it waits for a slot.
  const aggKey = JSON.stringify([version, params]);
  const [computing, setComputing] = useState<{ key: string; ahead: number | null } | null>(null);
  const agg = useQuery({
    queryKey: ["aggregate", version, params],
    queryFn: ({ signal }) => {
      // A new attempt starts as a plain search until the API says otherwise.
      setComputing((c) => (c?.key === aggKey ? null : c));
      return api.aggregate(params, version, signal, (ahead) => setComputing({ key: aggKey, ahead }));
    },
    enabled: !!version && !!view.q,
    // Keep showing the previous answer while the next loads only for the same
    // search in another bucket; a new search starts from a clear "searching"
    // state (#210). Never a result from another index version (it would be
    // drawn against this version's places).
    placeholderData: (prev, prevQuery) =>
      prev?.index_version === version && prevQuery && sameSearch(prevQuery.queryKey[2] as SearchParams, params)
        ? prev
        : undefined,
  });
  const baselineRef = agg.data?.cube.baseline_ref ?? null;
  const coverage = useQuery({
    queryKey: ["coverage", baselineRef],
    queryFn: ({ signal }) => api.coverage(baselineRef!, version, signal),
    enabled: !!baselineRef,
    staleTime: Infinity,
  });

  const data = agg.data;
  const stillComputing = agg.isFetching && computing?.key === aggKey;
  const ahead = stillComputing ? computing.ahead : null;
  const waitNote = !stillComputing
    ? null
    : ahead === null
      ? "Large search, still working… This can take up to two minutes."
      : ahead === 0
        ? "Other searches are running. Yours is next in line…"
        : `Other searches are running. Yours is in line behind ${ahead} more…`;
  const count = data?.bucket.count ?? 0;
  // Scrubbing and playback update the view at once but write the URL only
  // when movement pauses: browsers throttle the History API (Firefox allows
  // about 200 calls per 10 s), and a permalink only needs where it stopped.
  // `base` is the URL's `t` when scrubbing began; once the URL changes for
  // any reason (the deferred write, back/forward, a new search) it wins.
  // It also belongs to one search: a new search never inherits it.
  const key = searchKey(view);
  const [scrub, setScrub] = useState<{ key: string; base: string; t: string } | null>(null);
  const tIso = scrub && scrub.key === key && scrub.base === view.t ? scrub.t : view.t;
  const t = data ? (tIso ? bucketIndex(data.bucket.unit, data.bucket.from, count, tIso) : count - 1) : 0;
  const urlWrite = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(urlWrite.current), [key]);

  // Prefix sums are built once per response; each frame is O(places).
  const hitSums = useMemo(() => (data ? prefixSums(data.cube, data.places.id.length, count) : null), [data, count]);
  const pageSums = useMemo(() => {
    if (!data || !coverage.data || coverage.data.count !== count) return null;
    const aligned = alignCube(coverage.data.pages, coverage.data.places, data.places.id);
    return prefixSums(aligned, data.places.id.length, count);
  }, [data, coverage.data, count]);

  // Congress pages sit at one place of their own, which /v1/meta names
  // rather than /v1/places, so newspaper places stay as they are (#229).
  const congressPlace = meta.data?.congress?.place;
  const features = useMemo(() => {
    const byId = new Map((places.data?.features ?? []).map((f) => [f.id, f]));
    if (congressPlace) byId.set(congressPlace.id, congressFeature(congressPlace));
    return byId;
  }, [places.data, congressPlace]);

  // Relative rate (doc 11): fitted once per search, off the main thread;
  // each frame is then scored from prefix sums.
  const wantSkew = view.norm === "skew";
  // Once the view has been shown, keep its fit when switching to Pages and
  // back; until then don't fit at all.
  const [skewUsed, setSkewUsed] = useState(wantSkew);
  if (wantSkew && !skewUsed) setSkewUsed(true);
  const prepared: Prepared | Unavailable | null = useMemo(() => {
    if (!skewUsed || !data || !places.data) return null;
    // No baselines (a language filter on a version published before pages were
    // counted per language): there is no coverage to wait for (07 §7.9).
    if (data.cube.baseline_ref === null || data.series.baseline === null) return "filters";
    return coverage.data ? prepareSkew(data, coverage.data, features) : null;
  }, [skewUsed, data, coverage.data, places.data, features]);
  const { state: skew, last: lastSkew } = useSkewModel(typeof prepared === "object" ? prepared : null);
  // While a refit of the same search (and index version) runs, keep drawing
  // the previous fit rather than flipping to page counts and back. Another
  // search's scores must never be shown as this one's.
  const skewModel =
    skew.status === "ready"
      ? skew
      : skew.status === "computing" &&
          typeof prepared === "object" &&
          prepared !== null &&
          lastSkew?.prepared.search === prepared.search
        ? lastSkew
        : null;
  const frame = useMemo(() => (skewModel ? scoreFrame(skewModel.model, t, view.win) : null), [skewModel, t, view.win]);
  const maxExpected = useMemo(
    () => (skewModel ? maxWindowExpected(skewModel.model.places, view.win) : 1),
    [skewModel, view.win],
  );
  // Biggest circles first, so smaller ones are drawn on top of them. Circle size follows the
  // current frame's expected matches, so the order does too: in early cumulative frames and
  // trailing windows it differs from the whole search's.
  const drawOrder = useMemo(() => {
    if (!skewModel || !frame) return [];
    const units = skewModel.model.places.units;
    return Array.from({ length: units }, (_, u) => u).sort(
      (a, b) => (frame.places[b]?.expected ?? 0) - (frame.places[a]?.expected ?? 0),
    );
  }, [skewModel, frame]);
  const daysById = useMemo(
    () =>
      new Map(
        (data?.places.id ?? []).map((id, i) => [
          id,
          { firstDay: data!.places.first_day[i]!, lastDay: data!.places.last_day?.[i] ?? -1 },
        ]),
      ),
    [data],
  );
  const skewRows: (SkewRow & MapPoint & { firstDay: number; lastDay: number })[] = useMemo(() => {
    if (!skewModel || !frame) return [];
    const { placeIds, languages, languageCounts } = skewModel.prepared;
    return drawOrder.flatMap((i) => {
      const id = placeIds[i]!;
      const f = features.get(id);
      if (!f) return [];
      const info = skewInfo(frame.places[i]!, frame.placePages[i]!, languages[i] ?? null, languageCounts[i] ?? null);

      return [
        {
          id,
          name: f.properties.name,
          state: f.properties.state,
          precision: f.properties.precision,
          position: f.geometry.coordinates,
          value: info.observed,
          rel: info.pages > 0 ? info.observed / info.pages : Number.NaN,
          skew: info,
          firstDay: daysById.get(id)?.firstDay ?? -1,
          lastDay: daysById.get(id)?.lastDay ?? -1,
        },
      ];
    });
  }, [skewModel, frame, features, drawOrder, daysById]);
  // Lists, table, export and the announcement: places with pages in the frame.
  const skewListed = useMemo(() => skewRows.filter((r) => r.skew.pages > 0), [skewRows]);
  const stateRows = useMemo(
    () =>
      skewModel && frame
        ? skewModel.prepared.stateCodes.map((state, i) => ({
            state,
            skew: skewInfo(frame.states[i]!, frame.statePages[i]!, null),
          }))
        : [],
    [skewModel, frame],
  );

  // Relative frequency needs the coverage cube; until it arrives (or if it
  // fails) it is unknown (NaN), never 0.
  const points: (MapPoint & { firstDay: number; lastDay: number; middle: string })[] = useMemo(() => {
    if (!data || !hitSums) return [];
    const hits = windowValues(hitSums, t, view.win);
    // When each place's matches fell (#127): the median and middle half of
    // its pages in the window, by bucket.
    const median = windowQuantile(hitSums, t, view.win, 0.5);
    const q1 = windowQuantile(hitSums, t, view.win, 0.25);
    const q3 = windowQuantile(hitSums, t, view.win, 0.75);
    const label = (b: number) =>
      Number.isNaN(b) ? "" : bucketLabel(data.bucket.unit, bucketStart(data.bucket.unit, data.bucket.from, b));
    const span = Math.max(count - 1, 1);
    const rel = pageSums
      ? relative(hits, windowValues(pageSums, t, view.win))
      : new Float64Array(hits.length).fill(Number.NaN);
    return data.places.id.flatMap((id, i) => {
      const f = features.get(id);
      if (!f) return [];
      return [
        {
          id,
          name: f.properties.name,
          state: f.properties.state,
          precision: f.properties.precision,
          position: f.geometry.coordinates,
          value: hits[i]!,
          rel: rel[i]!,
          firstDay: data.places.first_day[i]!,
          lastDay: data.places.last_day?.[i] ?? -1,
          when: median[i]! / span,
          whenLabel: label(median[i]!),
          middle: q1[i] === q3[i] ? label(q1[i]!) : `${label(q1[i]!)} – ${label(q3[i]!)}`,
        },
      ];
    });
  }, [data, hitSums, pageSums, features, t, view.win, count]);

  // Scale circles to the largest value the playback will reach, so they
  // grow and shrink on one scale (cumulative: the totals; trailing: the
  // biggest window anywhere in the range). O(places × buckets), once.
  const maxValue = useMemo(() => {
    if (!hitSums) return 1;
    let max = 1;
    const buf = new Float64Array(hitSums.places);
    const ts = view.win === null ? [count - 1] : Array.from({ length: count }, (_, i) => i);
    for (const i of ts) for (const v of windowValues(hitSums, i, view.win, buf)) max = Math.max(max, v);
    return max;
  }, [hitSums, count, view.win]);
  // Colour by page counts until the relative rate is ready.
  const norm = view.norm === "skew" && !skewModel ? "raw" : view.norm;

  const urlT = view.t;
  const seek = useCallback(
    (i: number) => {
      if (!data) return;
      const iso = bucketStart(data.bucket.unit, data.bucket.from, i);
      setScrub((s) => ({ key, base: s && s.key === key && s.base === urlT ? s.base : urlT, t: iso }));
      clearTimeout(urlWrite.current);
      urlWrite.current = setTimeout(() => {
        setView({ t: iso });
        setScrub(null);
      }, 300);
    },
    [data, setView, urlT, key],
  );
  const select = useCallback((id: string) => setView({ place: id }), [setView]);
  const onViewport = useCallback((z: number, c: [number, number]) => setView({ z, c }), [setView]);
  const search = (patch: Partial<ViewState>) => setView({ ...patch, t: "", place: "", sort: "oldest" }, true);
  const placeName = (id: string) => {
    const f = features.get(id);
    return f ? `${f.properties.name}, ${f.properties.state}` : id;
  };

  const papers = useMemo(
    () =>
      paperRows(data?.papers, (id) => {
        const f = features.get(id);
        return f ? `${f.properties.name}, ${f.properties.state}` : id;
      }),
    [data?.papers, features],
  );
  // Under a newspaper filter the view shows Pages, not the relative rate (supportedNorm).
  const onlyPaper = (lccn: string) => setView({ lccn: [lccn], t: "", place: "", sort: "oldest" }, true);
  // Congress pages belong to no newspaper: the API's list is empty (#229).
  const newspapers =
    view.source !== "congress" && data && data.total.hits > 0 && data.papers ? (
      <NewspaperTable
        rows={papers}
        total={data.total.papers ?? papers.length}
        onOnly={onlyPaper}
        filename={`usnewsmap-newspapers-${version}.csv`}
      />
    ) : null;
  const days = data?.total.days;
  const mix = data
    ? [
        days !== undefined && data.total.hits > 0
          ? `Matching pages on ${days.toLocaleString("en-US")} ${days === 1 ? "day" : "days"}.`
          : null,
        languageMix(data.languages, data.total.hits, data.total.papers),
      ]
        .filter(Boolean)
        .join(" ") || null
    : null;
  const paperName = (lccn: string) =>
    titles.data?.titles.find((t) => t.lccn === lccn)?.title ?? papers.find((p) => p.lccn === lccn)?.title ?? lccn;
  const filteredPapers = view.lccn.length > 0 ? paperFilterNames(view.lccn, paperName) : null;

  const visible = points.filter((p) => p.value > 0);
  const pagesShown = visible.reduce((a, p) => a + p.value, 0);
  const pageCount = pagesShown.toLocaleString();
  // Pages only American Stories' text matched (#218), when the API says. Its
  // count is for the whole search, so only while the count shown is too.
  const asOnlyNote = data
    ? americanStoriesOnlyNote(data.total.american_stories_only, data.total.hits, pagesShown)
    : null;

  // How many places each side panel list shows. Kept here, above the lists, so
  // they keep it when they move (see `narrow` below), and because the Median
  // date lists' lengths decide which places' days are asked for.
  const [listShown, setListShown] = useListLengths(`${key}|${view.bucket}|${view.win}|${norm}`, LIST_NAMES);

  // Median date lists: the bucket only gives a month (or year), so the
  // places that could be listed, as far down as the lists show, get their
  // pages per day (/v1/days) and their exact median for the playback window.
  // The lists never show the bucket's ranking first and swap it for the
  // exact one: until the first answer for a search they say they are
  // loading, and after it a changed set (playback, scrubbing, a longer or
  // shorter list) keeps the last exact lists, marked as updating, until the
  // new answer. The first set, and the first after Show more or Show fewer,
  // is asked for at once; later ones once the set has held still for a
  // second, so playback doesn't send a request per step. Searches by day
  // need none: their buckets are days.
  const byDay = data?.bucket.unit === "day";
  const wantsDays = norm === "when" && !view.place && !!data && !byDay;
  const candidates = wantsDays && !agg.isPlaceholderData ? medianCandidates(visible, listShown) : [];
  const candidatesKey = candidates?.join(",") ?? "";
  // The first set for a search, or for new list lengths (Show more, Show
  // fewer), is asked for at once; any other set waits until it has held
  // still for a second. Kept per search and lengths during render, so the
  // first answer shows as soon as it arrives.
  const searchId = JSON.stringify([version, params]);
  const setId = `${searchId}|${listShown.earliest}|${listShown.latest}`;
  const [firstSet, setFirstSet] = useState({ setId: "", key: "" });
  if (candidatesKey !== "" && firstSet.setId !== setId) setFirstSet({ setId, key: candidatesKey });
  const isFirstSet = firstSet.setId === setId ? firstSet.key === candidatesKey : candidatesKey !== "";
  const [settledKey, setSettledKey] = useState("");
  useEffect(() => {
    const id = setTimeout(() => setSettledKey(candidatesKey), 1000);
    return () => clearTimeout(id);
  }, [candidatesKey]);
  const requestKey = isFirstSet ? candidatesKey : settledKey;
  const knownDays = useRef<{ searchId: string; places: Map<string, PlaceDays> }>({ searchId: "", places: new Map() });
  const dayCounts = useQuery({
    queryKey: ["days", version, params, requestKey],
    queryFn: async ({ signal }) => {
      // A search's days per place, kept across sets: a longer list, or the
      // same places at a new date, asks only for the places it adds.
      if (knownDays.current.searchId !== searchId) knownDays.current = { searchId, places: new Map() };
      const places = await daysFor(requestKey.split(","), knownDays.current.places, (ids) =>
        api.days(params, version, ids, signal),
      );
      return { places };
    },
    enabled: !!version && requestKey !== "" && requestKey === candidatesKey,
    staleTime: Infinity,
    retry: false,
    // While the next set loads, the last answer for this same search stays (marked as updating).
    placeholderData: (prev, prevQuery) =>
      prevQuery && JSON.stringify(prevQuery.queryKey.slice(1, 3)) === JSON.stringify([version, params])
        ? prev
        : undefined,
  });
  const fresh =
    !!dayCounts.data && !dayCounts.isPlaceholderData && candidatesKey !== "" && requestKey === candidatesKey;
  const baseStatus: MedianStatus = !wantsDays
    ? "bucket"
    : agg.isPlaceholderData
      ? "loading"
      : candidatesKey === ""
        ? "bucket"
        : fresh
          ? "exact"
          : dayCounts.isError && requestKey === candidatesKey
            ? "bucket"
            : dayCounts.data
              ? "updating"
              : "loading";
  // The search whose Median date lists have been shown. Once they have (by
  // bucket: too many tied places to ask for at the lengths shown), a first
  // answer still on its way, after Show more lets the days be asked for,
  // keeps them, marked as updating, rather than blanking them: the lists, and
  // the Show more button focus is on, stay mounted until the ranking settles.
  // Only a search that has shown no lists yet says it is loading. Empty
  // lists (no place yet has pages enough for a median) are not shown lists:
  // when playback makes a place eligible, its first ranking waits for the
  // exact answer rather than showing it by bucket first.
  const [listedFor, setListedFor] = useState("");
  const listed = baseStatus === "exact" || (baseStatus === "bucket" && (candidates === null || candidates.length > 0));
  if (wantsDays && listed && listedFor !== searchId) setListedFor(searchId);
  const medianStatus: MedianStatus =
    baseStatus === "loading" && !agg.isPlaceholderData && listedFor === searchId ? "updating" : baseStatus;
  const daysSource = dayCounts.data ?? null;
  const exactMedians = useMemo(() => {
    if (!data || !daysSource || (medianStatus !== "exact" && medianStatus !== "updating")) return null;
    const unit = data.bucket.unit;
    const from = data.bucket.from;
    const lo = view.win === null ? -Infinity : dayNumber(bucketStart(unit, from, Math.max(0, t - view.win + 1)));
    const hi = t + 1 >= count ? Infinity : dayNumber(bucketStart(unit, from, t + 1)) - 1;
    return new Map(daysSource.places.map((p) => [p.id, exactMedian(p.days, p.hits, lo, hi)]));
  }, [data, daysSource, medianStatus, view.win, t, count]);
  const selected = points.find((p) => p.id === view.place);
  const updating = [agg.error, places.error, coverage.error].some((e) => e instanceof VersionChangedError);
  const problem: Problem | null =
    updating || !agg.error
      ? null
      : agg.error instanceof ApiError
        ? agg.error.problem
        : {
            type: "about:blank",
            title: "The search failed",
            status: 0,
            detail: "The search service could not be reached or sent an unreadable response.",
            hint: "Check your connection and try again.",
          };
  const placesFailed = places.error && !(places.error instanceof VersionChangedError);
  const coverageFailed = view.norm === "skew" && coverage.error && !(coverage.error instanceof VersionChangedError);
  const skewNotice = wantSkew && !skewModel ? skewStatus(prepared, skew.status, !!coverageFailed) : null;
  const shown = norm === "skew" ? skewRows : points;
  const clear = norm === "skew" ? clearest(skewListed) : null;

  // The page lists of the places at each end of the side panel's lists,
  // loaded in the background once the lists are final (#265): after the
  // search's own answer (not a 202 or the last search's placeholder), the
  // relative rate's fit or its unavailability, and the exact median days.
  const listsFinal =
    !!data &&
    !!places.data &&
    !agg.isPlaceholderData &&
    !agg.isFetching &&
    !updating &&
    !view.place &&
    (view.norm !== "skew" ||
      norm === "skew" ||
      typeof prepared === "string" ||
      skew.status === "error" ||
      !!coverageFailed) &&
    (norm !== "when" || medianStatus === "exact" || medianStatus === "bucket");
  usePrefetchPlaceHits(
    params,
    version,
    view.sort,
    !listsFinal
      ? null
      : norm === "skew"
        ? { norm, rows: skewListed }
        : norm === "when"
          ? { norm, rows: visible, exact: medianStatus === "exact" ? exactMedians : null }
          : { norm: "raw", rows: visible },
  );

  // Phones show the legend and the place lists after the playback controls, in the DOM as well as
  // on screen, so the controls sit right under the map. They move, not the controls: the legend and
  // lists keep no state of their own, so a rotation across the breakpoint loses nothing. How many
  // places each list shows is kept in App (`listShown`, above) for that reason.
  const narrow = useMediaQuery("(max-width: 640px)");
  const legend = !data ? null : norm === "skew" && skewModel ? (
    <SkewLegend
      places={frame ? frame.placePages.filter((p) => p > 0).length : 0}
      states={frame ? frame.statePages.filter((p) => p > 0).length : 0}
      unit={data.bucket.unit}
    />
  ) : norm === "when" ? (
    <WhenLegend
      from={bucketLabel(data.bucket.unit, bucketStart(data.bucket.unit, data.bucket.from, 0))}
      to={bucketLabel(data.bucket.unit, bucketStart(data.bucket.unit, data.bucket.from, Math.max(count - 1, 0)))}
    />
  ) : (
    <Legend />
  );
  const sidePanel = !data ? null : (
    <>
      {norm === "skew" && !view.place && (
        <SkewLists rows={skewListed} onSelect={select} shown={listShown} onShown={setListShown} />
      )}
      {norm === "raw" && !view.place && (
        <PagesLists
          rows={visible}
          onSelect={select}
          trailing={view.win !== null}
          shown={listShown}
          onShown={setListShown}
        />
      )}
      {norm === "when" && !view.place && (
        <WhenLists
          rows={visible}
          onSelect={select}
          trailing={view.win !== null}
          exact={exactMedians}
          status={medianStatus}
          shown={listShown}
          onShown={setListShown}
          tied={candidates === null ? data.bucket.unit : undefined}
        />
      )}
      {view.place && (
        <PlacePanel
          params={params}
          version={version}
          placeId={view.place}
          placeName={selected?.name ?? features.get(view.place)?.properties.name ?? view.place}
          sort={view.sort}
          onSort={(sort) => setView({ sort })}
          windowHits={selected?.value ?? 0}
          note={norm === "skew" ? skewRows.find((r) => r.id === view.place)?.skew : undefined}
          synthetic={data.synthetic}
          onClose={() => setView({ place: "", sort: "oldest" })}
        />
      )}
    </>
  );

  return (
    <div className={view.q ? "app" : "app app--empty"}>
      <header className="topbar">
        <Brand />
        <SearchBar
          key={searchKey(view)}
          view={view}
          meta={meta.data}
          onSearch={search}
          busy={agg.isFetching}
          titles={titles.data?.titles}
          titlesFailed={titles.isError}
          onWantTitles={wantTitlesNow}
          onBrowsePaper={browsePaper}
        />
      </header>
      <About ref={about} />
      {browsing && meta.data?.browse_pages && (
        <PaperPages
          lccn={browsing.lccn}
          name={titles.data?.titles.find((t) => t.lccn === browsing.lccn)?.title ?? browsing.lccn}
          version={version}
          sort={browsing.sort}
          onSort={(sort) => setBrowsing({ ...browsing, sort })}
          synthetic={meta.data.synthetic}
          onClose={() => {
            setBrowsing(null);
            browsing.returnFocus();
          }}
        />
      )}

      {meta.data?.synthetic && (
        <p className="notice notice--demo" role="note">
          Demo data: this server is running on a small synthetic corpus, not real newspapers.
        </p>
      )}
      {meta.error && (
        <p className="notice notice--error" role="alert">
          The search service is unavailable. Please try again shortly.
        </p>
      )}

      {!view.q ? (
        <main className="empty">
          <h1>Where and when did America's newspapers print it?</h1>
          <p>Type a word or phrase to map every matching page from Chronicling America, then play it through time.</p>
          <Examples
            order={EXAMPLE_ORDER}
            clicks={exampleClicks}
            onMore={() => setExampleClicks((c) => c + 1)}
            onPick={(ex) => search({ ...ex.view })}
          />
        </main>
      ) : (
        <main className={agg.isFetching ? "results results--loading" : "results"}>
          {agg.isFetching && <div className="search-progress" role="progressbar" aria-label="Search running" />}
          {updating && (
            <p className="notice" role="status">
              A newer index was just published. Updating to it…
            </p>
          )}
          {waitNote && data && (
            <p className="notice" role="status">
              {waitNote}
            </p>
          )}
          {placesFailed && (
            <p className="notice notice--error" role="alert">
              Place locations could not be loaded, so results can't be mapped. Please reload the page.
            </p>
          )}
          {problem && (
            <div className="notice notice--error" role="alert">
              <strong>{problem.title}.</strong> {problem.detail}
              {problem.hint && <div>{problem.hint}</div>}
            </div>
          )}
          {data && !places.data && !placesFailed && (
            <p className="notice" role="status">
              Loading places…
            </p>
          )}
          {data && places.data && (
            <>
              <div className="toolbar">
                <p className="summary">
                  <strong>{visible.length.toLocaleString()}</strong> places ·{" "}
                  {asOnlyNote ? (
                    <Tooltip note={asOnlyNote}>
                      <strong>{pageCount}</strong> pages
                    </Tooltip>
                  ) : (
                    <>
                      <strong>{pageCount}</strong> pages
                    </>
                  )}
                  {data.coarsened && " · buckets coarsened to fit"}
                </p>
                {/* Options for one measure sit left of the controls every measure has, which keep
                    their place against the right edge whichever measure is chosen. */}
                <div className="toolbar__options">
                  {/* The relative-rate view draws points only (doc 11, 11.6), and the layer means
                      nothing while the table is shown (the Table view, or no WebGL). */}
                  {view.norm === "raw" && view.tab === "map" && webgl && (
                    <label>
                      <span className="visually-hidden">Map layer</span>
                      <select
                        value={view.layer}
                        onChange={(e) => setView({ layer: e.target.value as ViewState["layer"] })}
                      >
                        <option value="points">Points</option>
                        <option value="heat">Heat</option>
                      </select>
                    </label>
                  )}
                  {norm === "skew" && data && (
                    <DownloadCsv
                      rows={skewListed}
                      filename={`usnewsmap-relative-rate-${version}-${bucketStart(data.bucket.unit, data.bucket.from, t)}.csv`}
                    />
                  )}
                </div>
                <div className="segmented toolbar__view" role="group" aria-label="View">
                  {(["map", "table"] as const).map((tab) => (
                    <button key={tab} type="button" aria-pressed={view.tab === tab} onClick={() => setView({ tab })}>
                      {tab === "map" ? "Map" : "Table"}
                    </button>
                  ))}
                </div>
                <MeasureToggle
                  norm={view.norm}
                  onChange={(n) => setView({ norm: n })}
                  baseline={baselineNote(data?.baseline)}
                  unavailable={
                    // Both show Pages (supportedNorm); the button says why (#229).
                    view.source === "congress"
                      ? { skew: "Not for Congress pages yet: they have no pages-published counts." }
                      : view.lccn.length > 0
                        ? {
                            skew:
                              view.lccn.length === 1
                                ? "Not for one newspaper: pages published aren't counted per newspaper."
                                : "Not for chosen newspapers: pages published aren't counted per newspaper.",
                          }
                        : undefined
                  }
                />
                <ShareButton />
              </div>

              {mix && <p className="mix">{mix}</p>}
              {filteredPapers && (
                <p className="notice" role="status">
                  Only pages from{" "}
                  {filteredPapers.names.map((name, i) => (
                    <Fragment key={i}>
                      {i > 0 && (i === filteredPapers.names.length - 1 && !filteredPapers.more ? " and " : ", ")}
                      <cite>{name}</cite>
                    </Fragment>
                  ))}
                  {filteredPapers.more > 0 && ` and ${filteredPapers.more} more`}
                  {view.lccn.length === 1 ? ", so the map shows its place only" : ""}.{" "}
                  <button
                    type="button"
                    className="link-button"
                    onClick={() => setView({ lccn: [], t: "", place: "", sort: "oldest" }, true)}
                  >
                    Show every newspaper
                  </button>
                </p>
              )}
              {/* Not while the previous search stands in: its pages would get this search's links. */}
              {data.total.hits > 0 && data.total.first && !agg.isPlaceholderData && (
                <Mentions
                  first={data.total.first}
                  last={data.total.last ?? null}
                  placeName={placeName}
                  hrefFor={(hit, sort) =>
                    `${window.location.pathname}${serializeView({ ...view, place: hit.place_id, sort })}`
                  }
                  onOpen={(hit, sort) => setView({ place: hit.place_id, sort })}
                />
              )}
              {skewNotice && (
                <p
                  className={skewNotice.error ? "notice notice--error" : "notice"}
                  role={skewNotice.error ? "alert" : "status"}
                >
                  {skewNotice.text}
                </p>
              )}
              {data.total.hits === 0 ? (
                <div className="notice" role="status">
                  <strong>No pages match.</strong>{" "}
                  {/* A Congress search has no state or language filters to remove (#229). */}
                  {view.source === "congress"
                    ? "Try “All words” instead of an exact phrase, or widen the dates."
                    : `Try “All words” instead of an exact phrase, widen the dates, or remove ${
                        view.lccn.length > 0
                          ? "newspaper, language or state filters"
                          : view.lang.length > 0
                            ? "language or state filters"
                            : "state filters"
                      }.`}
                </div>
              ) : (
                // Busy is scoped to the stale map or table, not <main>: the status
                // messages above stay live while a search runs.
                <div className="stage" aria-busy={agg.isFetching}>
                  {view.tab === "table" || !webgl ? (
                    <>
                      {!webgl && view.tab === "map" && (
                        <p className="notice" role="note">
                          The map needs WebGL2, which this browser doesn't provide. Showing the table instead.
                        </p>
                      )}
                      {norm === "skew" ? (
                        <div className="tables">
                          <PlaceTable key="skew" skew rows={skewListed} onSelect={select} selected={view.place} />
                          <StateTable rows={stateRows} />
                          {newspapers}
                        </div>
                      ) : (
                        <div className="tables">
                          <PlaceTable
                            // A new sort when the share column comes or goes, so it never sorts by a hidden column.
                            key={data.cube.baseline_ref !== null ? "raw" : "raw-no-share"}
                            rows={visible}
                            onSelect={select}
                            selected={view.place}
                            share={data.cube.baseline_ref !== null}
                          />
                          {newspapers}
                        </div>
                      )}
                    </>
                  ) : (
                    <Suspense fallback={<div className="map map--loading">Loading map…</div>}>
                      <MapView
                        points={shown}
                        layer={view.norm === "raw" ? view.layer : "points"}
                        norm={norm}
                        maxValue={norm === "skew" ? maxExpected : maxValue}
                        selected={view.place}
                        onSelect={select}
                        zoom={view.z}
                        center={view.c}
                        onViewport={onViewport}
                      />
                      {!narrow && legend}
                    </Suspense>
                  )}
                  {!narrow && sidePanel}
                </div>
              )}

              {count > 0 && data.total.hits > 0 && (
                <Announcer
                  message={
                    clear
                      ? `Relative rate for ${skewListed.length.toLocaleString()} places ${
                          view.win === null ? "up to" : "in the window ending"
                        } ${bucketLabel(data.bucket.unit, bucketStart(data.bucket.unit, data.bucket.from, t))}: ${skewListed.filter((r) => r.skew.dir === 1).length} clearly above 1×, ${
                          skewListed.filter((r) => r.skew.dir === -1).length
                        } clearly below.`
                      : `Showing ${visible.length.toLocaleString()} places, ${visible
                          .reduce((a, p) => a + p.value, 0)
                          .toLocaleString()} pages, up to ${bucketLabel(
                          data.bucket.unit,
                          bucketStart(data.bucket.unit, data.bucket.from, t),
                        )}.`
                  }
                />
              )}
              {count > 0 && data.total.hits > 0 && (
                <footer className="timebar">
                  <TimeDock
                    unit={data.bucket.unit}
                    from={data.bucket.from}
                    count={count}
                    t={t}
                    window={view.win}
                    onSeek={seek}
                    onWindow={(win) => setView({ win })}
                  />
                  <Timeline
                    unit={data.bucket.unit}
                    from={data.bucket.from}
                    hits={data.series.hits}
                    t={t}
                    window={view.win}
                    onSeek={seek}
                  />
                </footer>
              )}
              {narrow && data && (
                <div className="after-timebar">
                  {view.tab === "map" && webgl && legend}
                  {sidePanel}
                </div>
              )}
            </>
          )}
          {!data && agg.isFetching && <Searching q={view.q} note={waitNote} />}
        </main>
      )}
      <footer className="credits">
        Newspaper pages from{" "}
        <a href="https://chroniclingamerica.loc.gov/" target="_blank" rel="noopener noreferrer">
          Chronicling America
        </a>{" "}
        (
        <a href="https://www.loc.gov/ndnp/" target="_blank" rel="noopener noreferrer">
          NEH and Library of Congress
        </a>
        ).
        {meta.data && ` ${indexSummary(meta.data)}`}{" "}
        <button type="button" className="link-button" aria-haspopup="dialog" onClick={openAbout}>
          About
        </button>{" "}
        · <a href="/datasets">Dataset details</a> · <a href="/privacy">Privacy</a>
      </footer>
    </div>
  );
}

/**
 * The one live region for results (07 §7.6): the date and counts together,
 * announced at most every 1.5 s, so playback doesn't flood a screen reader.
 */
function Announcer({ message }: { message: string }) {
  const [spoken, setSpoken] = useState("");
  const last = useRef(0);
  useEffect(() => {
    const wait = Math.max(0, last.current + 1500 - Date.now());
    const timer = setTimeout(() => {
      last.current = Date.now();
      setSpoken(message);
    }, wait);
    return () => clearTimeout(timer);
  }, [message]);
  return (
    <p className="visually-hidden" role="status" aria-live="polite">
      {spoken}
    </p>
  );
}

function Legend() {
  return (
    <div className="legend">
      <div className="legend__head">
        <div className="legend__title">Pages containing the match</div>
        <InfoTip label="About this legend" up>
          <p>Circle area is proportional to a place's matching pages.</p>
          <p>A hollow ring is a place known only to its county or state.</p>
        </InfoTip>
      </div>
      <div className="legend__ramp" aria-hidden="true">
        {[0, 0.25, 0.5, 0.75, 1].map((x) => (
          <span key={x} style={{ background: cssColor(x) }} />
        ))}
      </div>
      <div className="legend__ends" aria-hidden="true">
        <span>fewer</span>
        <span>more</span>
      </div>
    </div>
  );
}

/** The median-date view's legend (#127): the search's first and last buckets. */
function WhenLegend({ from, to }: { from: string; to: string }) {
  return (
    <div className="legend">
      <div className="legend__head">
        <div className="legend__title">Median date of the matching pages</div>
        <InfoTip label="About this legend" up>
          <p>A place&apos;s colour is the period by whose end half its matching pages had appeared.</p>
          <p>Circle area is proportional to its matching pages.</p>
        </InfoTip>
      </div>
      <div className="legend__ramp" aria-hidden="true">
        {[0, 0.25, 0.5, 0.75, 1].map((x) => (
          <span key={x} style={{ background: cssTimeColor(x) }} />
        ))}
      </div>
      <div className="legend__ends" aria-hidden="true">
        <span>{from}</span>
        <span>{to}</span>
      </div>
    </div>
  );
}

function ShareButton() {
  return (
    <button
      type="button"
      className="button toolbar__share"
      onClick={(e) => {
        const btn = e.currentTarget;
        void navigator.clipboard?.writeText(window.location.href).then(
          () => {
            btn.textContent = "Link copied";
            setTimeout(() => (btn.textContent = "Share"), 2000);
          },
          () => undefined,
        );
      }}
    >
      Share
    </button>
  );
}

/** Why the relative rate isn't showing yet, or can't be shown. */
function skewStatus(
  prepared: Prepared | Unavailable | null,
  status: "idle" | "computing" | "ready" | "error",
  coverageFailed: boolean,
): { text: string; error: boolean } {
  if (coverageFailed)
    return {
      text: "Publication counts could not be loaded, so the relative rate isn't available. Showing page counts.",
      error: true,
    };
  if (prepared === "filters")
    return {
      text: "The relative rate isn't available with a language filter on this version of the index. Showing page counts.",
      error: false,
    };
  if (prepared === "few-places")
    return {
      text: `The relative rate needs at least ${MIN_PLACES} places with pages between these dates. Showing page counts.`,
      error: false,
    };
  if (status === "error")
    return { text: "The relative rate could not be worked out. Showing page counts.", error: true };
  if (prepared === "mismatch")
    return {
      text: "Publication counts don't match this search's time buckets, so the relative rate isn't available. Showing page counts.",
      error: true,
    };
  if (prepared === null) return { text: "Loading publication counts…", error: false };
  return { text: "Comparing places…", error: false };
}
