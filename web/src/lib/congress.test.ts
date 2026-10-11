import { describe, expect, it } from "vitest";
import { congressFeature, hitTitle } from "./congress";

describe("congressFeature", () => {
  it("places Congress at its own point, longitude first", () => {
    const f = congressFeature({ id: "PC001", name: "Congress (Washington, DC)", lat: 38.89, lon: -77.01 });
    expect(f.id).toBe("PC001");
    expect(f.geometry.coordinates).toEqual([-77.01, 38.89]);
    expect(f.properties.state).toBe("DC");
  });
});

describe("hitTitle", () => {
  it("names a newspaper page by its title, else its LCCN, as before", () => {
    expect(hitTitle({ title: "The Sun", lccn: "sn83030272" })).toBe("The Sun");
    expect(hitTitle({ title: null, lccn: "sn83030272" })).toBe("sn83030272");
  });

  it("names a Congress page by its title, else its citation (#229)", () => {
    const congress = { series: "serialset" as const, citation: "H. Doc. 16-42" };
    expect(hitTitle({ title: "Report on the Mint", lccn: "", congress })).toBe("Report on the Mint");
    expect(hitTitle({ title: null, lccn: "", congress })).toBe("H. Doc. 16-42");
  });
});
