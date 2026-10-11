import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { Meta } from "../api/types";
import { DEFAULTS } from "../state/url";
import { SearchBar } from "./SearchBar";

afterEach(cleanup);

const meta = {
  bounds: { from: "1736-01-01", to: "1963-12-31" },
  limits: { max_query_chars: 256 },
  ja: null,
} as unknown as Meta;

const withCongress = {
  ...meta,
  congress: {
    series: [{ series: "serialset", pages: 1, from: "1817-12-01", to: "1994-12-31" }],
    place: { id: "PC001", name: "Congress", lat: 38.89, lon: -77.01 },
  },
} as Meta;

describe("SearchBar's name (#229)", () => {
  it("names the corpus the form searches", () => {
    render(<SearchBar view={{ ...DEFAULTS, q: "gold", source: "congress" }} meta={withCongress} onSearch={() => {}} />);
    expect(screen.getByRole("search", { name: "Search Congress" })).toBeTruthy();
    fireEvent.change(screen.getByRole("combobox", { name: "Search" }), { target: { value: "news" } });
    expect(screen.getByRole("search", { name: "Search newspapers" })).toBeTruthy();
  });

  it("stays on newspapers when /v1/meta lists no Congress pages", () => {
    // The view asks for Congress, but the form can only search newspapers.
    render(<SearchBar view={{ ...DEFAULTS, q: "gold", source: "congress" }} meta={meta} onSearch={() => {}} />);
    expect(screen.getByRole("search", { name: "Search newspapers" })).toBeTruthy();
    cleanup();
    render(<SearchBar view={{ ...DEFAULTS, q: "gold" }} meta={withCongress} onSearch={() => {}} />);
    expect(screen.getByRole("search", { name: "Search newspapers" })).toBeTruthy();
  });
});

describe("SearchBar's options count (#229)", () => {
  const toggle = () => screen.getByRole("button", { name: /^Options/ });

  it("counts the source the form searches, not one /v1/meta doesn't offer", () => {
    render(<SearchBar view={{ ...DEFAULTS, q: "gold", source: "congress" }} meta={meta} onSearch={() => {}} />);
    expect(toggle().textContent).toBe("Options");
  });

  it("leaves out the filters a Congress search hides", () => {
    render(
      <SearchBar
        view={{ ...DEFAULTS, q: "gold", state: ["GA"], lang: ["ger"] }}
        meta={withCongress}
        onSearch={() => {}}
      />,
    );
    expect(toggle().textContent).toBe("Options (2)");
    fireEvent.change(screen.getByRole("combobox", { name: "Search" }), { target: { value: "congress" } });
    expect(toggle().textContent).toBe("Options (1)");
  });
});

describe("SearchBar before /v1/meta has loaded (#229)", () => {
  const congressLink = { ...DEFAULTS, q: "gold", source: "congress" as const };

  it("keeps a Congress link's source, so an early submit searches Congress", () => {
    const onSearch = vi.fn();
    render(<SearchBar view={congressLink} meta={undefined} onSearch={onSearch} />);
    expect(screen.getByRole("search", { name: "Search Congress" })).toBeTruthy();
    fireEvent.submit(screen.getByRole("search"));
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ source: "congress", state: [], lang: [] }));
  });

  it("opens a Congress link's options, and keeps them open once /v1/meta lists Congress", () => {
    const { rerender } = render(<SearchBar view={congressLink} meta={undefined} onSearch={() => {}} />);
    const toggle = () => screen.getByRole("button", { name: /^Options/ });
    expect(toggle().getAttribute("aria-expanded")).toBe("true");
    rerender(<SearchBar view={congressLink} meta={withCongress} onSearch={() => {}} />);
    expect(toggle().getAttribute("aria-expanded")).toBe("true");
    expect(toggle().textContent).toBe("Options (1)");
  });
});
