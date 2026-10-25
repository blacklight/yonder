// Mastodon-style `Link` header pagination: the API returns
// `<url>; rel="next"` (and sometimes `prev`) on list endpoints.

export interface PageLinks {
  next?: string;
  prev?: string;
}

export function parseLinkHeader(header: string | null): PageLinks {
  const links: PageLinks = {};
  if (!header) return links;
  for (const part of header.split(",")) {
    const match = part.match(/<([^>]+)>\s*;\s*rel="(next|prev)"/);
    if (!match) continue;
    const [, url, rel] = match;
    if (rel === "next") links.next = url;
    if (rel === "prev") links.prev = url;
  }
  return links;
}

export interface Page<T> {
  items: T[];
  next?: string;
  prev?: string;
}

export function paginated<T>(response: Response, items: T[]): Page<T> {
  const links = parseLinkHeader(response.headers.get("link"));
  return { items, next: links.next, prev: links.prev };
}
