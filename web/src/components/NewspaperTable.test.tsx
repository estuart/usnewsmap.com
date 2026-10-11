import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { NewspaperTable, paperRows, papersCsv } from "./NewspaperTable";
import { languageMix } from "../lib/languages";

const names: Record<string, string> = { P00001: "Chicago, IL", P00002: "Omaha, NE" };
const placeName = (id: string) => names[id] ?? id;

describe("newspapers (#121)", () => {
  afterEach(cleanup);

  it("joins place names and falls back to the LCCN for an uncatalogued title", () => {
    const rows = paperRows(
      { lccn: ["sn1", "sn2"], hits: [5, 3], title: ["The Eagle", null], place_id: ["P00001", null] },
      placeName,
    );
    expect(rows).toEqual([
      { lccn: "sn1", title: "The Eagle", place: "Chicago, IL", hits: 5 },
      { lccn: "sn2", title: "sn2", place: "", hits: 3 },
    ]);
    expect(paperRows(undefined, placeName)).toEqual([]);
  });

  it("quotes CSV cells that need it", () => {
    expect(papersCsv([{ lccn: "sn1", title: 'The "Eagle", Daily', place: "Chicago, IL", hits: 1200 }])).toBe(
      'lccn,newspaper,place,matching_pages\r\nsn1,"The ""Eagle"", Daily","Chicago, IL",1200\r\n',
    );
  });

  it("lists the first 25, shows all on request, and limits the search to one paper", () => {
    const rows = Array.from({ length: 30 }, (_, i) => ({
      lccn: `sn${i}`,
      title: `Paper ${i}`,
      place: "Omaha, NE",
      hits: 30 - i,
    }));
    const onOnly = vi.fn();
    render(<NewspaperTable rows={rows} total={812} onOnly={onOnly} filename="x.csv" />);
    expect(screen.getByText(/812 newspapers have matching pages/)).toBeTruthy();
    expect(screen.getByText(/the 30 with the most are listed/)).toBeTruthy();
    expect(screen.getAllByRole("row")).toHaveLength(1 + 25);
    fireEvent.click(screen.getByRole("button", { name: "Show all 30" }));
    expect(screen.getAllByRole("row")).toHaveLength(1 + 30);
    fireEvent.click(screen.getByRole("button", { name: "Only this newspaper: Paper 3" }));
    expect(onOnly).toHaveBeenCalledWith("sn3");
    // Its issues by date are at the Library of Congress.
    expect(screen.getByRole("link", { name: "Issues of Paper 3 at loc.gov" }).getAttribute("href")).toBe(
      "https://www.loc.gov/item/sn3/",
    );
  });

  it("describes the language mix only when there is more than one language", () => {
    expect(languageMix({ code: ["eng", "ger", "spa"], hits: [960, 30, 4] }, 1000, 312)).toBe(
      "Matches in 312 newspapers: English 96%, German 3% and Spanish under 1%.",
    );
    expect(languageMix({ code: ["eng"], hits: [10] }, 10, 2)).toBeNull();
    expect(languageMix(undefined, 10, 2)).toBeNull();
    const many = { code: ["eng", "ger", "spa", "fre", "ita", "pol", "cze"], hits: [70, 10, 5, 5, 4, 3, 3] };
    expect(languageMix(many, 100, 1)).toBe(
      "Matches in 1 newspaper: English 70%, German 10%, Spanish 5%, French 5%, Italian 4% and 2 more.",
    );
  });
});
