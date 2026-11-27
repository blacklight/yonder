import { describe, expect, it, vi } from "vitest";
import {
  fetchRemoteDomainBlocks,
  instanceOrigin,
  normalizeRemoteDomain,
  parseDomainBlocksEntity,
  parseInstanceMrf,
  remoteBlocksCsv,
  RemoteDomainBlocksError,
} from "./remoteDomainBlocks";

function response(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("remoteDomainBlocks", () => {
  it("normalizes an instance input to an http origin", () => {
    expect(instanceOrigin("mastodon.example/path")).toBe(
      "https://mastodon.example",
    );
    expect(instanceOrigin("https://mastodon.example/about")).toBe(
      "https://mastodon.example",
    );
    expect(instanceOrigin("http://localhost:3000")).toBe(
      "http://localhost:3000",
    );
    expect(() => instanceOrigin("not a host")).toThrow(RemoteDomainBlocksError);
  });

  it("normalizes remote domains and rejects obfuscated wildcards", () => {
    expect(normalizeRemoteDomain("https://Example.COM/users/a")).toBe(
      "example.com",
    );
    expect(normalizeRemoteDomain("*.example.com")).toBe("example.com");
    expect(normalizeRemoteDomain("exam***.com")).toBe("");
  });

  it("parses the Mastodon domain-blocks entity shape", () => {
    expect(
      parseDomainBlocksEntity([
        {
          domain: "https://Spam.example/x",
          digest: "x",
          severity: "suspend",
          comment: "spam",
        },
        { domain: "noise.example", severity: "silence", comment: "noise" },
        { domain: "noise.example", severity: "noop", comment: "weaker" },
        { domain: "hidden***.example", severity: "suspend" },
      ]),
    ).toEqual([
      { domain: "noise.example", severity: "silence", comment: "noise" },
      { domain: "spam.example", severity: "suspend", comment: "spam" },
    ]);
  });

  it("parses Pleroma/Akkoma MRF metadata and reasons", () => {
    const instance = {
      pleroma: {
        metadata: {
          federation: {
            mrf_simple: {
              reject: ["spam.example", "*.wild.example"],
              federated_timeline_removal: ["noise.example", "spam.example"],
              media_removal: ["ignored.example"],
            },
            mrf_simple_info: {
              reject: {
                "spam.example": { reason: "spam" },
                "*.wild.example": { reason: "wild" },
              },
              federated_timeline_removal: {
                "noise.example": { reason: "noise" },
              },
            },
          },
        },
      },
    };
    expect(parseInstanceMrf(instance)).toEqual([
      { domain: "noise.example", severity: "silence", comment: "noise" },
      { domain: "spam.example", severity: "suspend", comment: "spam" },
      { domain: "wild.example", severity: "suspend", comment: "wild" },
    ]);
  });

  it("parses tuple-shaped older MRF entries", () => {
    const instance = {
      pleroma: {
        metadata: {
          federation: {
            mrf_simple: {
              reject: [["spam.example", "inline spam"]],
              federated_timeline_removal: [["noise.example", "inline noise"]],
            },
          },
        },
      },
    };
    expect(parseInstanceMrf(instance)).toEqual([
      { domain: "noise.example", severity: "silence", comment: "inline noise" },
      { domain: "spam.example", severity: "suspend", comment: "inline spam" },
    ]);
  });

  it("uses the dedicated domain-blocks endpoint first", async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValue(
        response([{ domain: "spam.example", severity: "suspend" }]),
      );
    const result = await fetchRemoteDomainBlocks("source.example", fetcher);
    expect(result.source).toBe("domain_blocks");
    expect(result.blocks).toEqual([
      { domain: "spam.example", severity: "suspend", comment: "" },
    ]);
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(fetcher.mock.calls[0][0]).toBe(
      "https://source.example/api/v1/instance/domain_blocks",
    );
  });

  it("falls back to instance metadata when the endpoint is unavailable", async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(response({}, 404))
      .mockResolvedValueOnce(
        response({
          pleroma: {
            metadata: {
              federation: {
                mrf_simple: { reject: ["spam.example"] },
              },
            },
          },
        }),
      );
    const result = await fetchRemoteDomainBlocks("source.example", fetcher);
    expect(result.source).toBe("instance");
    expect(result.blocks[0].severity).toBe("suspend");
    expect(fetcher.mock.calls[1][0]).toBe(
      "https://source.example/api/v1/instance",
    );
  });

  it("reports a failure when neither API is usable", async () => {
    const fetcher = vi.fn().mockResolvedValue(response({}, 500));
    await expect(
      fetchRemoteDomainBlocks("source.example", fetcher),
    ).rejects.toMatchObject({ code: "fetch_failed" });
  });

  it("serializes remote choices to the extended import CSV", () => {
    const csv = remoteBlocksCsv(
      [
        {
          domain: "spam.example",
          severity: "suspend",
          comment: 'spam, "really"',
        },
        { domain: "noise.example", severity: "silence", comment: "noise" },
      ],
      {
        severityMode: "source",
        sourceHost: "source.example",
      },
    );
    expect(csv).toContain(
      "#domain,#severity,#reject_media,#reject_reports,#public_comment,#private_comment,#obfuscate",
    );
    expect(csv).toContain(
      'spam.example,suspend,false,false,"spam, ""really""",Imported from source.example,false',
    );
    expect(csv).toContain(
      "noise.example,silence,false,false,noise,Imported from source.example,false",
    );
  });

  it("can force one severity and one custom reason for every row", () => {
    const csv = remoteBlocksCsv(
      [
        { domain: "spam.example", severity: "suspend", comment: "spam" },
        { domain: "noise.example", severity: "silence", comment: "noise" },
      ],
      {
        severityMode: "silence",
        comment: "reviewed import",
        sourceHost: "source.example",
      },
    );
    expect(csv).toContain(
      "spam.example,silence,false,false,reviewed import,Imported from source.example,false",
    );
    expect(csv).toContain(
      "noise.example,silence,false,false,reviewed import,Imported from source.example,false",
    );
  });
});
