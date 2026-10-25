import { describe, expect, it } from "vitest";
import { parseLinkHeader, paginated } from "./pagination";

describe("parseLinkHeader", () => {
  it("parses next and prev links", () => {
    const links = parseLinkHeader(
      '<https://x/api/v1/timelines/home?max_id=2>; rel="next", <https://x/api/v1/timelines/home?min_id=9>; rel="prev"',
    );
    expect(links.next).toContain("max_id=2");
    expect(links.prev).toContain("min_id=9");
  });
  it("handles a missing header", () => {
    expect(parseLinkHeader(null)).toEqual({});
  });
  it("ignores other rels", () => {
    const links = parseLinkHeader('<https://x/other>; rel="alternate"');
    expect(links).toEqual({});
  });
});

describe("paginated", () => {
  it("extracts links from a response", () => {
    const response = new Response("[]", {
      headers: { link: '</x?page=2>; rel="next"' },
    });
    const page = paginated(response, [1, 2]);
    expect(page.items).toEqual([1, 2]);
    expect(page.next).toBe("/x?page=2");
    expect(page.prev).toBeUndefined();
  });
});
