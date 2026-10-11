import { describe, expect, it } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { LIST_LENGTH, MOST_SHOWN, useListLengths } from "./ListLength";

describe("useListLengths", () => {
  it("starts every named list at the first few, sets each on its own, and resets them all on a new key", () => {
    expect([LIST_LENGTH, MOST_SHOWN]).toEqual([5, 25]);
    const names = ["above", "below", "places", "states"] as const;
    const { result, rerender } = renderHook(({ k }) => useListLengths(k, names), { initialProps: { k: "a" } });
    expect(result.current[0]).toEqual({ above: 5, below: 5, places: 5, states: 5 });
    act(() => result.current[1]("places", 15));
    act(() => result.current[1]("above", 10));
    expect(result.current[0]).toEqual({ above: 10, below: 5, places: 15, states: 5 });
    // The same search, window and measure: kept.
    rerender({ k: "a" });
    expect(result.current[0].places).toBe(15);
    // A new one: every list back to the first few.
    rerender({ k: "b" });
    expect(result.current[0]).toEqual({ above: 5, below: 5, places: 5, states: 5 });
  });
});
