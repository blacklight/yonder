import { describe, expect, it } from "vitest";
import { formatRelativeDate } from "./relativeDate";

const NOW = new Date("2026-06-15T12:00:00Z");
const ago = (ms: number) => new Date(NOW.getTime() - ms).toISOString();

describe("formatRelativeDate", () => {
  it("seconds under a minute", () => {
    expect(formatRelativeDate(ago(0), NOW, "en")).toBe("0s");
    expect(formatRelativeDate(ago(59_000), NOW, "en")).toBe("59s");
  });

  it("minutes under an hour", () => {
    expect(formatRelativeDate(ago(60_000), NOW, "en")).toBe("1m");
    expect(formatRelativeDate(ago(59 * 60_000), NOW, "en")).toBe("59m");
  });

  it("hours under a day", () => {
    expect(formatRelativeDate(ago(3600_000), NOW, "en")).toBe("1h");
    expect(formatRelativeDate(ago(23 * 3600_000), NOW, "en")).toBe("23h");
  });

  it("days under a week", () => {
    expect(formatRelativeDate(ago(24 * 3600_000), NOW, "en")).toBe("1d");
    expect(formatRelativeDate(ago(6 * 24 * 3600_000), NOW, "en")).toBe("6d");
  });

  it("month + day under a year", () => {
    const out = formatRelativeDate(ago(30 * 24 * 3600_000), NOW, "en");
    expect(out).toMatch(/^[A-Z][a-z]{2} \d{1,2}$/);
  });

  it("month, day, year when older", () => {
    const out = formatRelativeDate(ago(400 * 24 * 3600_000), NOW, "en");
    expect(out).toMatch(/^[A-Z][a-z]{2} \d{1,2}, \d{4}$/);
  });
});
