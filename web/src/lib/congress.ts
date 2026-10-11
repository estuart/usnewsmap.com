import type { CongressMeta, CongressSeries, HitItem, PlaceFeature } from "../api/types";

/**
 * The map feature for the one place every Congress page sits at for now
 * (#229): Washington, DC, from `/v1/meta`, kept apart from DC's newspapers.
 */
export function congressFeature(place: CongressMeta["place"]): PlaceFeature {
  return {
    type: "Feature",
    id: place.id,
    geometry: { type: "Point", coordinates: [place.lon, place.lat] },
    properties: {
      name: place.name,
      state: "DC",
      precision: "city",
      titles: 0,
      languages: ["eng"],
    },
  };
}

/** Where a Congress page's viewer link opens, by series (04 §4.10). */
export const CONGRESS_SITES: Record<CongressSeries, string> = {
  serialset: "GovInfo",
  globe: "the UNT Digital Library",
  annals: "Congress.gov",
  register: "Congress.gov",
};

/**
 * A page's title in lists: its newspaper's, or a Congress page's document
 * or volume, else its citation, as a Congress page has no LCCN (#229).
 */
export function hitTitle(h: Pick<HitItem, "title" | "lccn" | "congress">): string {
  return h.title ?? h.congress?.citation ?? h.lccn;
}
