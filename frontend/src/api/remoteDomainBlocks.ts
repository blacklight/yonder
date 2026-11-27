/**
 * Public domain-block discovery on another instance.
 *
 * Mastodon-compatible servers expose `/api/v1/instance/domain_blocks`.
 * Older Pleroma/Akkoma releases instead publish their MRF SimplePolicy
 * transparency data in `/api/v1/instance` under
 * `{pleroma,akkoma}.metadata.federation`. The browser fetches both
 * endpoints directly when the remote instance publishes them. No local
 * credentials are ever sent to the remote host.
 */

export type RemoteDomainBlockSeverity = "noop" | "silence" | "suspend";

export interface RemoteDomainBlock {
  domain: string;
  severity: RemoteDomainBlockSeverity;
  comment: string;
}

export interface RemoteDomainBlocksResult {
  blocks: RemoteDomainBlock[];
  source: "domain_blocks" | "instance";
  origin: string;
}

export class RemoteDomainBlocksError extends Error {
  constructor(public readonly code: "invalid_instance" | "fetch_failed") {
    super(code);
    this.name = "RemoteDomainBlocksError";
  }
}

type Fetcher = typeof fetch;

/** Canonical http(s) origin for a user-entered instance host or URL. */
export function instanceOrigin(input: string): string {
  const trimmed = input.trim();
  if (!trimmed) throw new RemoteDomainBlocksError("invalid_instance");
  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed)
    ? trimmed
    : `https://${trimmed}`;
  try {
    const url = new URL(withScheme);
    if (
      (url.protocol !== "https:" && url.protocol !== "http:") ||
      url.username ||
      url.password
    ) {
      throw new Error("invalid");
    }
    return url.origin;
  } catch {
    throw new RemoteDomainBlocksError("invalid_instance");
  }
}

/**
 * Browser-side equivalent of the backend domain canonicalizer for the
 * shapes remote instances publish. Obfuscated wildcard names are not
 * importable and are intentionally rejected.
 */
export function normalizeRemoteDomain(value: string): string {
  let candidate = value.trim();
  if (!candidate || /\s/.test(candidate)) return "";
  // Akkoma accepts a leading `*.` although its blocks already match
  // subdomains. Any other wildcard is an obfuscation, not a blockable
  // hostname.
  if (candidate.startsWith("*.")) candidate = candidate.slice(2);
  if (candidate.includes("*")) return "";
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(candidate)) {
    candidate = `https://${candidate}`;
  }
  try {
    let host = new URL(candidate).hostname.toLowerCase();
    if (host.startsWith("[") && host.endsWith("]")) {
      host = host.slice(1, -1);
    }
    return host.replace(/\.+$/, "");
  } catch {
    return "";
  }
}

function severity(value: unknown): RemoteDomainBlockSeverity | null {
  if (typeof value !== "string") return null;
  switch (value.trim().toLowerCase()) {
    case "noop":
    case "none":
      return "noop";
    case "silence":
    case "silenced":
    case "limit":
    case "limited":
    case "federated_timeline_removal":
      return "silence";
    case "suspend":
    case "suspended":
    case "reject":
    case "rejected":
      return "suspend";
    default:
      return null;
  }
}

const rank = { noop: 1, silence: 2, suspend: 3 } as const;

function mergeBlock(
  out: Map<string, RemoteDomainBlock>,
  domain: string,
  nextSeverity: RemoteDomainBlockSeverity,
  comment: string,
) {
  if (!domain) return;
  const existing = out.get(domain);
  if (!existing || rank[nextSeverity] >= rank[existing.severity]) {
    out.set(domain, {
      domain,
      severity: nextSeverity,
      comment: comment || existing?.comment || "",
    });
  } else if (!existing.comment && comment) {
    existing.comment = comment;
  }
}

function sorted(out: Map<string, RemoteDomainBlock>): RemoteDomainBlock[] {
  return [...out.values()].sort((a, b) => a.domain.localeCompare(b.domain));
}

/** Parse the Mastodon `/api/v1/instance/domain_blocks` entity array. */
export function parseDomainBlocksEntity(
  value: unknown,
): RemoteDomainBlock[] | null {
  if (!Array.isArray(value)) return null;
  const out = new Map<string, RemoteDomainBlock>();
  for (const item of value) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    const parsedSeverity = severity(row.severity);
    if (!parsedSeverity) continue;
    mergeBlock(
      out,
      normalizeRemoteDomain(typeof row.domain === "string" ? row.domain : ""),
      parsedSeverity,
      typeof row.comment === "string" ? row.comment : "",
    );
  }
  return sorted(out);
}

function objectGet(value: unknown, key: string): unknown {
  return value && typeof value === "object"
    ? (value as Record<string, unknown>)[key]
    : undefined;
}

function mrfEntries(value: unknown): Array<[string, string]> {
  if (!Array.isArray(value)) return [];
  const entries: Array<[string, string]> = [];
  for (const item of value) {
    if (typeof item === "string") {
      entries.push([item, ""]);
    } else if (Array.isArray(item) && typeof item[0] === "string") {
      entries.push([item[0], typeof item[1] === "string" ? item[1] : ""]);
    } else if (item && typeof item === "object") {
      const row = item as Record<string, unknown>;
      const domain = row.instance ?? row.domain ?? row.host;
      if (typeof domain === "string") {
        entries.push([
          domain,
          typeof row.reason === "string" ? row.reason : "",
        ]);
      }
    }
  }
  return entries;
}

function mrfInfoReason(
  info: unknown,
  category: string,
  domain: string,
): string {
  const row = objectGet(objectGet(info, category), domain);
  const reason = objectGet(row, "reason");
  return typeof reason === "string" ? reason : "";
}

/**
 * Extract the two block-shaped SimplePolicy categories exposed by
 * Pleroma/Akkoma instance metadata. `reject` is a suspension and
 * `federated_timeline_removal` is a silence — the same mapping their
 * own `/instance/domain_blocks` implementations use.
 */
export function parseInstanceMrf(value: unknown): RemoteDomainBlock[] {
  const pleroma = objectGet(value, "pleroma") ?? objectGet(value, "akkoma");
  const federation = objectGet(objectGet(pleroma, "metadata"), "federation");
  const simple = objectGet(federation, "mrf_simple");
  if (!simple || typeof simple !== "object") return [];
  const info = objectGet(federation, "mrf_simple_info");
  const out = new Map<string, RemoteDomainBlock>();
  for (const [category, mapped] of [
    ["reject", "suspend"],
    ["federated_timeline_removal", "silence"],
  ] as const) {
    for (const [rawDomain, inlineReason] of mrfEntries(
      objectGet(simple, category),
    )) {
      const domain = normalizeRemoteDomain(rawDomain);
      const reason =
        inlineReason ||
        mrfInfoReason(info, category, domain) ||
        mrfInfoReason(info, category, rawDomain);
      mergeBlock(out, domain, mapped, reason);
    }
  }
  return sorted(out);
}

async function fetchJson(fetcher: Fetcher, url: string) {
  const response = await fetcher(url, {
    headers: { Accept: "application/json" },
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

/**
 * Fetch public limitations from `input`, trying the dedicated
 * domain-blocks endpoint first and Pleroma/Akkoma instance metadata as
 * the compatibility fallback.
 */
export async function fetchRemoteDomainBlocks(
  input: string,
  fetcher: Fetcher = globalThis.fetch.bind(globalThis),
): Promise<RemoteDomainBlocksResult> {
  const origin = instanceOrigin(input);
  try {
    const data = await fetchJson(
      fetcher,
      `${origin}/api/v1/instance/domain_blocks`,
    );
    const blocks = parseDomainBlocksEntity(data);
    if (blocks) return { blocks, source: "domain_blocks", origin };
  } catch {
    // Older Pleroma/Akkoma versions do not have this endpoint, and
    // Mastodon can hide it behind a local user token. The public
    // instance document is the compatibility fallback either way.
  }

  try {
    const data = await fetchJson(fetcher, `${origin}/api/v1/instance`);
    return {
      blocks: parseInstanceMrf(data),
      source: "instance",
      origin,
    };
  } catch {
    throw new RemoteDomainBlocksError("fetch_failed");
  }
}

export type RemoteSeverityMode = "source" | RemoteDomainBlockSeverity;

function csvCell(value: string): string {
  return /[",\n\r]/.test(value) ? `"${value.replaceAll('"', '""')}"` : value;
}

function capComment(value: string): string {
  return [...value].slice(0, 2000).join("");
}

/**
 * Serialize the user's modal choices to the extended Mastodon CSV shape
 * accepted by the local bulk importer. The private note preserves where
 * the entries came from; the public reason is either the operator's
 * custom text or the reason published by the source instance.
 */
export function remoteBlocksCsv(
  blocks: RemoteDomainBlock[],
  options: {
    severityMode: RemoteSeverityMode;
    comment?: string;
    sourceHost: string;
  },
): string {
  const lines = [
    "#domain,#severity,#reject_media,#reject_reports,#public_comment,#private_comment,#obfuscate",
  ];
  const customComment = options.comment?.trim() ?? "";
  const privateComment = capComment(`Imported from ${options.sourceHost}`);
  for (const block of blocks) {
    const appliedSeverity =
      options.severityMode === "source" ? block.severity : options.severityMode;
    const publicComment = capComment(customComment || block.comment);
    lines.push(
      [
        block.domain,
        appliedSeverity,
        "false",
        "false",
        publicComment,
        privateComment,
        "false",
      ]
        .map(csvCell)
        .join(","),
    );
  }
  return `${lines.join("\n")}\n`;
}
