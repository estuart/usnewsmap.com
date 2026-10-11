import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { Meta } from "../api/types";
import { DEFAULTS } from "../state/url";
import { SearchBar } from "./SearchBar";

afterEach(cleanup);

const meta = (ja: Meta["ja"]) =>
  ({ bounds: { from: "1736-01-01", to: "1963-12-31" }, limits: { max_query_chars: 256 }, ja }) as unknown as Meta;

describe("SearchBar with Japanese", () => {
  it("says what a Japanese query searches", () => {
    render(
      <SearchBar
        view={{ ...DEFAULTS, q: "東京" }}
        meta={meta({ indexes: ["pages-ja-x"], fold: 1, pages: 11058 })}
        onSearch={() => {}}
      />,
    );
    const box = screen.getByRole("searchbox");
    const hint = screen.getByRole("note");
    expect(hint.textContent).toBe(
      "Japanese searches cover only the 11,058 Japanese-language pages we read ourselves. The Library of Congress has no searchable text for them.",
    );
    expect(box.getAttribute("aria-describedby")).toBe(hint.id);
  });

  it("says nothing before /v1/meta has loaded", () => {
    render(<SearchBar view={{ ...DEFAULTS, q: "東京" }} meta={undefined} onSearch={() => {}} />);
    expect(screen.queryByRole("note")).toBeNull();
  });

  it("says when Japanese search isn't there yet, and nothing for other queries", () => {
    const { rerender } = render(
      <SearchBar view={{ ...DEFAULTS, q: "真珠湾" }} meta={meta(null)} onSearch={() => {}} />,
    );
    expect(screen.getByRole("note").textContent).toMatch(/isn't available yet/);
    rerender(<SearchBar key="b" view={{ ...DEFAULTS, q: "pearl harbor" }} meta={meta(null)} onSearch={() => {}} />);
    expect(screen.queryByRole("note")).toBeNull();
    expect(screen.getByRole("searchbox").getAttribute("aria-describedby")).toBeNull();
  });

  it("says a Congress search can't take Japanese text (#229)", () => {
    const withCongress = {
      ...meta({ indexes: ["pages-ja-x"], fold: 1, pages: 11058 }),
      congress: {
        series: [{ series: "globe", pages: 1, from: "1833-12-02", to: "1873-03-03" }],
        place: { id: "PC001", name: "Congress", lat: 38.89, lon: -77.01 },
      },
    } as Meta;
    render(<SearchBar view={{ ...DEFAULTS, q: "東京", source: "congress" }} meta={withCongress} onSearch={() => {}} />);
    const hint = screen.getByRole("note");
    expect(hint.textContent).toBe("Japanese text can't be searched in Congress pages.");
    expect(screen.getByRole("searchbox").getAttribute("aria-describedby")).toBe(hint.id);
    // Back to the newspapers, the newspaper hint as before.
    fireEvent.change(screen.getByRole("combobox", { name: "Search" }), { target: { value: "news" } });
    expect(screen.getByRole("note").textContent).toMatch(/^Japanese searches cover only the 11,058/);
  });

  it("doesn't search on the Enter that confirms an input method's word", () => {
    const onSearch = vi.fn();
    render(<SearchBar view={{ ...DEFAULTS, q: "とうきょう" }} meta={meta(null)} onSearch={onSearch} />);
    const box = screen.getByRole("searchbox");
    fireEvent.compositionStart(box);
    expect(fireEvent.keyDown(box, { key: "Enter" })).toBe(false); // default prevented
    fireEvent.compositionEnd(box);
    // Safari: the composition has ended, but the confirming Enter says keyCode 229.
    expect(fireEvent.keyDown(box, { key: "Enter", keyCode: 229 })).toBe(false);
    // A plain Enter afterwards searches as usual.
    expect(fireEvent.keyDown(box, { key: "Enter" })).toBe(true);
    fireEvent.submit(box.closest("form")!);
    expect(onSearch).toHaveBeenCalledTimes(1);
  });
});
