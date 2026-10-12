import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { Meta } from "../api/types";
import { PHONE } from "../lib/useMediaQuery";
import { DEFAULTS, type ViewState } from "../state/url";
import { SearchBar } from "./SearchBar";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  delete (HTMLInputElement.prototype as { showPicker?: unknown }).showPicker;
});

const meta = {
  bounds: { from: "1736-01-01", to: "1963-12-31" },
  limits: { max_query_chars: 256 },
  ja: null,
} as unknown as Meta;

/** Lay the form out as on a phone, where the Options button holds every option. */
function onPhone() {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query === PHONE,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
}

function setup(view: Partial<ViewState> = {}) {
  const onSearch = vi.fn();
  render(<SearchBar view={{ ...DEFAULTS, q: "gold", ...view }} meta={meta} onSearch={onSearch} />);
  const from = screen.getByRole("textbox", { name: "From" }) as HTMLInputElement;
  const to = screen.getByRole("textbox", { name: "To" }) as HTMLInputElement;
  const submit = () => fireEvent.submit(from.closest("form")!);
  return { onSearch, from, to, submit };
}

describe("SearchBar dates", () => {
  it("searches a year from January 1 to December 31 and shows the dates", () => {
    const { onSearch, from, to, submit } = setup();
    fireEvent.change(from, { target: { value: "1827" } });
    fireEvent.change(to, { target: { value: "1827" } });
    submit();
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ from: "1827-01-01", to: "1827-12-31" }));
    expect(from.value).toBe("01/01/1827");
    expect(to.value).toBe("12/31/1827");
  });

  it("searches a month from its first day to its last", () => {
    const { onSearch, from, to, submit } = setup();
    fireEvent.change(from, { target: { value: "3/1860" } });
    fireEvent.change(to, { target: { value: "02/1860" } });
    submit();
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ from: "1860-03-01", to: "1860-02-29" }));
  });

  it("fills in the date on leaving the box", () => {
    const { from, to } = setup();
    fireEvent.change(to, { target: { value: "04/1900" } });
    fireEvent.blur(to);
    expect(to.value).toBe("04/30/1900");
    expect(to.getAttribute("aria-invalid")).toBeNull();
    fireEvent.change(from, { target: { value: "__/__/1900" } });
    fireEvent.blur(from);
    expect(from.value).toBe("01/01/1900");
  });

  it("shows the URL's dates as mm/dd/yyyy and keeps them", () => {
    const { onSearch, from, to, submit } = setup({ from: "1895-01-01", to: "1897-12-31" });
    expect(from.value).toBe("01/01/1895");
    expect(to.value).toBe("12/31/1897");
    submit();
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ from: "1895-01-01", to: "1897-12-31" }));
  });

  it("refuses a day and a year without a month, and says so next to the box", () => {
    const { onSearch, from, to, submit } = setup();
    fireEvent.change(from, { target: { value: "15/1860" } });
    submit();
    expect(onSearch).not.toHaveBeenCalled();
    expect(from.getAttribute("aria-invalid")).toBe("true");
    const error = document.getElementById(from.getAttribute("aria-describedby")!.split(" ")[1]!)!;
    expect(error.textContent).toBe("From: Add the month, or enter just the year.");
    expect(document.activeElement).toBe(from);
    expect(to.getAttribute("aria-invalid")).toBeNull();
    // Typing again clears the message; a good entry then searches.
    fireEvent.change(from, { target: { value: "1860" } });
    expect(from.getAttribute("aria-invalid")).toBeNull();
    expect(screen.queryByText(/Add the month/)).toBeNull();
    submit();
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ from: "1860-01-01", to: "" }));
  });

  it("checks a box on leaving it and focuses To when only To is wrong", () => {
    const { onSearch, to, submit } = setup();
    fireEvent.change(to, { target: { value: "gold" } });
    fireEvent.blur(to);
    expect(screen.getByText("To: Enter a date as mm/dd/yyyy, mm/yyyy or yyyy.")).toBeTruthy();
    expect(to.value).toBe("gold");
    submit();
    expect(onSearch).not.toHaveBeenCalled();
    expect(document.activeElement).toBe(to);
  });

  it("keeps dates inside the index, as the date picker's limits did", () => {
    const { onSearch, from, to, submit } = setup();
    fireEvent.change(from, { target: { value: "06/01/1700" } });
    fireEvent.change(to, { target: { value: "1963" } });
    submit();
    expect(onSearch).not.toHaveBeenCalled();
    expect(screen.getByText("From: Enter a date between 01/01/1736 and 12/31/1963.")).toBeTruthy();
    fireEvent.change(from, { target: { value: "1736" } });
    submit();
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ from: "1736-01-01", to: "1963-12-31" }));
  });

  it("keeps a year as typed until the index's days are known, then cuts it to them", () => {
    const onSearch = vi.fn();
    const later = { ...meta, bounds: { from: "1736-03-05", to: "1963-10-31" } } as Meta;
    const { rerender } = render(<SearchBar view={{ ...DEFAULTS, q: "gold" }} meta={undefined} onSearch={onSearch} />);
    const from = screen.getByRole("textbox", { name: "From" }) as HTMLInputElement;
    const to = screen.getByRole("textbox", { name: "To" }) as HTMLInputElement;
    fireEvent.change(from, { target: { value: "1736" } });
    fireEvent.blur(from);
    fireEvent.change(to, { target: { value: "10/1963" } });
    fireEvent.blur(to);
    expect(from.value).toBe("1736");
    expect(to.value).toBe("10/1963");
    expect(from.getAttribute("aria-invalid")).toBeNull();
    // /v1/meta loads: the index starts on 03/05/1736 and ends on 10/31/1963.
    rerender(<SearchBar view={{ ...DEFAULTS, q: "gold" }} meta={later} onSearch={onSearch} />);
    fireEvent.blur(from);
    expect(from.value).toBe("03/05/1736");
    fireEvent.submit(from.closest("form")!);
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ from: "1736-03-05", to: "1963-10-31" }));
    expect(to.value).toBe("10/31/1963");
  });

  it("searches a year before the index's days are known without filling the box in", () => {
    const onSearch = vi.fn();
    render(<SearchBar view={{ ...DEFAULTS, q: "gold" }} meta={undefined} onSearch={onSearch} />);
    const from = screen.getByRole("textbox", { name: "From" }) as HTMLInputElement;
    fireEvent.change(from, { target: { value: "1736" } });
    fireEvent.submit(from.closest("form")!);
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ from: "1736-01-01" }));
    expect(from.value).toBe("1736");
  });

  it("opens the options on phones to show a problem", () => {
    onPhone();
    const { from, submit } = setup();
    const toggle = screen.getByRole("button", { name: /^Options/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    fireEvent.change(from, { target: { value: "/15/1860" } });
    submit();
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
  });

  it("counts a typed date as an option on phones, where the dates are behind the button", () => {
    onPhone();
    const { from } = setup();
    const toggle = screen.getByRole("button", { name: /^Options/ });
    expect(toggle.textContent).toBe("Options");
    fireEvent.change(from, { target: { value: "1860" } });
    expect(toggle.textContent).toBe("Options (1)");
  });

  it("leaves the dates out of the count on wide screens, where they stay in the row", () => {
    const { from, submit } = setup();
    const toggle = screen.getByRole("button", { name: /^Options/ });
    fireEvent.change(from, { target: { value: "1860" } });
    expect(toggle.textContent).toBe("Options");
    // A date problem doesn't open the filters: the box is already in view.
    fireEvent.change(from, { target: { value: "/15/1860" } });
    submit();
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(from);
  });

  it("opens the browser's calendar from the button and takes the date picked", () => {
    const showPicker = vi.fn();
    (HTMLInputElement.prototype as { showPicker?: unknown }).showPicker = showPicker;
    const { onSearch, to, submit } = setup();
    fireEvent.change(to, { target: { value: "1860" } });
    fireEvent.click(screen.getByRole("button", { name: "Choose the To date on a calendar" }));
    expect(showPicker).toHaveBeenCalledTimes(1);
    const picker = to.parentElement!.querySelector<HTMLInputElement>("input[type=date]")!;
    // The picker starts on the date the box stands for, within the index.
    expect(picker.value).toBe("1860-12-31");
    expect(picker.min).toBe("1736-01-01");
    expect(picker.max).toBe("1963-12-31");
    expect(picker.getAttribute("aria-hidden")).toBe("true");
    expect(picker.tabIndex).toBe(-1);
    fireEvent.change(picker, { target: { value: "1860-11-06" } });
    expect(to.value).toBe("11/06/1860");
    submit();
    expect(onSearch).toHaveBeenCalledWith(expect.objectContaining({ to: "1860-11-06" }));
  });

  it("has no calendar button where the browser can't open the picker", () => {
    setup();
    expect(screen.queryByRole("button", { name: /calendar/ })).toBeNull();
  });
});
