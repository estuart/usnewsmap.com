import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { Meta } from "../api/types";
import { PHONE } from "../lib/useMediaQuery";
import { DEFAULTS, type ViewState } from "../state/url";
import { SearchBar, activeFilters } from "./SearchBar";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

/** A matchMedia whose phone query can be switched, as when a phone is turned sideways. */
function viewport(phone: boolean) {
  const listeners = new Set<() => void>();
  vi.stubGlobal("matchMedia", (query: string) => ({
    get matches() {
      return query === PHONE && phone;
    },
    media: query,
    addEventListener: (_: string, f: () => void) => listeners.add(f),
    removeEventListener: (_: string, f: () => void) => listeners.delete(f),
  }));
  return (next: boolean) => {
    phone = next;
    act(() => listeners.forEach((f) => f()));
  };
}

const meta = {
  bounds: { from: "1736-01-01", to: "1963-12-31" },
  limits: { max_query_chars: 256 },
  ja: null,
  languages: [
    { code: "eng", name: "English", titles: 4, pages: 100 },
    { code: "ger", name: "German", titles: 2, pages: 10 },
  ],
} as unknown as Meta;

function setup(view: Partial<ViewState> = {}) {
  render(<SearchBar view={{ ...DEFAULTS, q: "gold", ...view }} meta={meta} onSearch={() => {}} />);
  const toggle = screen.getByRole("button", { name: /^Options/ });
  const panel = document.getElementById(toggle.getAttribute("aria-controls")!)!;
  return { toggle, panel };
}

describe("activeFilters", () => {
  it("counts the states, languages and newspapers, not the match mode or dates", () => {
    expect(activeFilters(DEFAULTS)).toBe(0);
    expect(activeFilters({ ...DEFAULTS, state: ["GA"], lang: ["ger"], lccn: ["sn1", "sn2"] })).toBe(3);
  });

  it("counts a Congress search once, without the filters it drops", () => {
    expect(activeFilters({ ...DEFAULTS, source: "congress", state: ["GA"] })).toBe(1);
  });
});

describe("SearchBar's Options button on wide screens", () => {
  it("keeps the match mode and dates in the row and folds the filters into a closed panel", () => {
    const { toggle, panel } = setup();
    expect(toggle.textContent).toBe("Options");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(panel.hidden).toBe(true);
    expect(screen.getByRole("combobox", { name: "Match" })).toBeTruthy();
    expect(screen.getByRole("textbox", { name: "From" })).toBeTruthy();
    expect(screen.queryByRole("textbox", { name: "States" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Languages/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Newspapers/ })).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(panel.hidden).toBe(false);
    expect(screen.getByRole("textbox", { name: "States" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Languages: any" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Newspapers: any" })).toBeTruthy();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(panel.hidden).toBe(true);
  });

  it("shows how many filters a link sets, with the panel closed", () => {
    const { toggle, panel } = setup({ state: ["GA", "SC"], lang: ["ger"], lccn: ["sn1"] });
    expect(toggle.textContent).toBe("Options (3)");
    expect(panel.hidden).toBe(true);
  });

  it("counts filters as they're set", () => {
    const { toggle } = setup();
    fireEvent.click(toggle);
    fireEvent.change(screen.getByRole("textbox", { name: "States" }), { target: { value: "GA" } });
    expect(toggle.textContent).toBe("Options (1)");
    fireEvent.click(screen.getByRole("button", { name: "Languages: any" }));
    fireEvent.click(screen.getByRole("checkbox", { name: /^German/ }));
    expect(toggle.textContent).toBe("Options (2)");
    // The match mode stays in the row and isn't counted.
    fireEvent.change(screen.getByRole("combobox", { name: "Match" }), { target: { value: "all" } });
    expect(toggle.textContent).toBe("Options (2)");
  });

  it("closes on Escape and puts focus back on the button", () => {
    const { toggle, panel } = setup();
    fireEvent.click(toggle);
    const states = screen.getByRole("textbox", { name: "States" });
    states.focus();
    fireEvent.keyDown(states, { key: "Escape" });
    expect(panel.hidden).toBe(true);
    expect(document.activeElement).toBe(toggle);
    // Escape on the button itself closes it too.
    fireEvent.click(toggle);
    fireEvent.keyDown(toggle, { key: "Escape" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });

  it("lets the language list take the first Escape", () => {
    const { toggle, panel } = setup();
    fireEvent.click(toggle);
    const languages = screen.getByRole("button", { name: "Languages: any" });
    fireEvent.click(languages);
    fireEvent.keyDown(screen.getByRole("checkbox", { name: /^German/ }), { key: "Escape" });
    expect(languages.getAttribute("aria-expanded")).toBe("false");
    expect(panel.hidden).toBe(false);
    fireEvent.keyDown(languages, { key: "Escape" });
    expect(panel.hidden).toBe(true);
  });

  it("doesn't close for Escape in the dates, which are outside the panel", () => {
    const { toggle, panel } = setup();
    fireEvent.click(toggle);
    fireEvent.keyDown(screen.getByRole("textbox", { name: "From" }), { key: "Escape" });
    expect(panel.hidden).toBe(false);
  });

  it("closes on a press outside it, not inside it", () => {
    const { toggle, panel } = setup();
    fireEvent.click(toggle);
    fireEvent.pointerDown(screen.getByRole("textbox", { name: "States" }));
    expect(panel.hidden).toBe(false);
    fireEvent.pointerDown(screen.getByRole("searchbox"));
    expect(panel.hidden).toBe(true);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });

  it("opens to show States when the browser finds it invalid on submit", () => {
    const { toggle, panel } = setup();
    fireEvent.click(toggle);
    const states = screen.getByRole("textbox", { name: "States" });
    fireEvent.change(states, { target: { value: "Georgia" } });
    // Pressing Search is a press outside the panel, so it closes first.
    fireEvent.pointerDown(screen.getByRole("button", { name: "Search" }));
    expect(panel.hidden).toBe(true);
    fireEvent.invalid(states);
    expect(panel.hidden).toBe(false);
  });
});

describe("SearchBar's Options button across the phone breakpoint", () => {
  it("closes the wide-screen panel when the layout changes", () => {
    const resize = viewport(true);
    const { toggle } = setup({ from: "1890-01-01" });
    // A phone opens with its options showing: the search uses a date.
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(toggle.textContent).toBe("Options (1)");
    resize(false);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(toggle.textContent).toBe("Options");
    resize(true);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
  });

  it("stays closed on a phone when no option is in use", () => {
    const resize = viewport(false);
    const { toggle } = setup();
    fireEvent.click(toggle);
    resize(true);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });
});
