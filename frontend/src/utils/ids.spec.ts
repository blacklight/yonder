import { describe, expect, it } from "vitest";
import { compareIds } from "./ids";

describe("compareIds", () => {
  it("orders snowflake strings numerically without Number()", () => {
    // Both exceed Number.MAX_SAFE_INTEGER — Number() would round
    // them onto the same double and report them equal.
    const a = "117519765199651274";
    const b = "117519765199651280";
    expect(Number(a)).toBe(Number(b));
    expect(compareIds(a, b)).toBeLessThan(0);
    expect(compareIds(b, a)).toBeGreaterThan(0);
  });

  it("orders by length before lexicographically", () => {
    expect(compareIds("999", "1000")).toBeLessThan(0);
    expect(compareIds("42", "42")).toBe(0);
  });
});
