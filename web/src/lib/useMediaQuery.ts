import { useCallback, useSyncExternalStore } from "react";

/** The phone layout's breakpoint, as in styles.css. */
export const PHONE = "(max-width: 640px)";

/** Whether a CSS media query matches, kept current as the viewport changes. */
export function useMediaQuery(query: string): boolean {
  // Stable per query, so re-renders (every playback frame) don't re-subscribe the listener.
  const subscribe = useCallback(
    (onChange: () => void) => {
      if (typeof window.matchMedia !== "function") return () => {};
      const list = window.matchMedia(query);
      list.addEventListener("change", onChange);
      return () => list.removeEventListener("change", onChange);
    },
    [query],
  );
  // Without matchMedia (jsdom in unit tests), no query matches.
  const snapshot = useCallback(
    () => typeof window.matchMedia === "function" && window.matchMedia(query).matches,
    [query],
  );
  return useSyncExternalStore(subscribe, snapshot, () => false);
}
