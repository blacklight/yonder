import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import AdminDomainBlocksPage from "./AdminDomainBlocksPage.vue";

function json(body: unknown) {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

const importResult = {
  created: 2,
  updated: 1,
  skipped_existing: 3,
  skipped_invalid: 1,
  duplicates: 0,
  total_rows: 7,
  errors: [],
};

describe("AdminDomainBlocksPage CSV import/export", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("renders the CSV transfer controls", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation(() => json([]));
    const { wrapper } = mountWith(AdminDomainBlocksPage);
    await flushPromises();
    expect(wrapper.text()).toContain("Import / export");
    expect(wrapper.text()).toContain("Mastodon's domain-block format");
    expect(wrapper.text()).toContain("Export CSV");
    expect(wrapper.text()).toContain("Import CSV");
    expect(wrapper.text()).toContain("Update existing blocks");
  });

  it("downloads the exported CSV", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementationOnce(() => json([]))
      .mockImplementationOnce(() =>
        Promise.resolve(
          new Response("csv", {
            status: 200,
            headers: {
              "Content-Disposition": 'attachment; filename="domain_blocks.csv"',
            },
          }),
        ),
      );
    const { wrapper } = mountWith(AdminDomainBlocksPage);
    await flushPromises();
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    const createObjectUrl = vi
      .spyOn(URL, "createObjectURL")
      .mockReturnValue("blob:x");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});

    const button = wrapper
      .findAll("button")
      .find((b) => b.text().includes("Export CSV"));
    await button!.trigger("click");
    await flushPromises();

    expect(fetchSpy.mock.calls[1][0]).toBe(
      "/api/v1/admin/domain_blocks/export",
    );
    expect(createObjectUrl).toHaveBeenCalled();
  });

  it("uploads the selected CSV, sends overwrite and refreshes the list", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementationOnce(() => json([]))
      .mockImplementationOnce(() => json(importResult))
      .mockImplementationOnce(() => json([]));
    const { wrapper } = mountWith(AdminDomainBlocksPage);
    await flushPromises();

    await wrapper.find(".blocks__overwrite input").setValue(true);
    const input = wrapper.find('input[type="file"]');
    const file = new File(["bad.example\n"], "blocks.csv", {
      type: "text/csv",
    });
    Object.defineProperty(input.element, "files", {
      value: [file],
      configurable: true,
    });
    await input.trigger("change");
    await flushPromises();

    const [url, init] = fetchSpy.mock.calls[1] as [string, RequestInit];
    expect(url).toBe("/api/v1/admin/domain_blocks/import");
    expect(init.method).toBe("POST");
    const form = init.body as FormData;
    expect((form.get("file") as File).name).toBe("blocks.csv");
    expect(form.get("overwrite")).toBe("true");
    expect(fetchSpy.mock.calls[2][0]).toBe("/api/v1/admin/domain_blocks");
    expect(wrapper.find(".blocks__import-summary").text()).toContain(
      "2 created",
    );
    expect(wrapper.find(".blocks__import-summary").text()).toContain(
      "1 invalid rows skipped",
    );
  });

  it("fetches remote blocks, selects rows and imports customized choices", async () => {
    const remoteBlocks = [
      { domain: "spam.example", severity: "suspend", comment: "spam" },
      { domain: "noise.example", severity: "silence", comment: "noise" },
    ];
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation((input) => {
        const url = String(input);
        if (url === "/api/v1/admin/domain_blocks") return json([]);
        if (url === "https://source.example/api/v1/instance/domain_blocks") {
          return json(remoteBlocks);
        }
        if (url === "/api/v1/admin/domain_blocks/import") {
          return json({ ...importResult, created: 1, updated: 0 });
        }
        return Promise.reject(new Error(`unexpected fetch ${url}`));
      });
    const { wrapper } = mountWith(AdminDomainBlocksPage);
    await flushPromises();

    const open = wrapper
      .findAll("button")
      .find((b) => b.text().includes("Import from instance"));
    await open!.trigger("click");
    await flushPromises();
    const dialog = document.querySelector<HTMLElement>("[role='dialog']");
    expect(dialog).toBeTruthy();

    const textInput = () =>
      document.querySelector<HTMLInputElement>(
        ".modal__panel .remote__fetch input[type='text']",
      )!;
    textInput().value = "source.example";
    textInput().dispatchEvent(new Event("input", { bubbles: true }));
    const fetchButton = [
      ...document.querySelectorAll<HTMLButtonElement>(".modal__panel button"),
    ].find((b) => b.textContent?.includes("Fetch blocks"))!;
    fetchButton.click();
    await flushPromises();

    const rowChecks = () => [
      ...document.querySelectorAll<HTMLInputElement>(
        ".remote__row input[type='checkbox']",
      ),
    ];
    expect(rowChecks()).toHaveLength(2);
    expect(rowChecks().every((input) => input.checked)).toBe(true);

    const deselect = [
      ...document.querySelectorAll<HTMLButtonElement>(".modal__panel button"),
    ].find((b) => b.textContent?.includes("Deselect all"))!;
    deselect.click();
    await flushPromises();
    expect(rowChecks().every((input) => !input.checked)).toBe(true);

    const selectAll = document.querySelector<HTMLInputElement>(
      ".remote__selection input[type='checkbox']",
    )!;
    selectAll.checked = true;
    selectAll.dispatchEvent(new Event("change", { bubbles: true }));
    await flushPromises();
    expect(rowChecks().every((input) => input.checked)).toBe(true);

    // Deselect one row manually: it must not be present in the generated CSV.
    rowChecks()[0].checked = false;
    rowChecks()[0].dispatchEvent(new Event("change", { bubbles: true }));

    const severity = document.querySelector<HTMLSelectElement>(
      ".remote__field select",
    )!;
    severity.value = "silence";
    severity.dispatchEvent(new Event("change", { bubbles: true }));
    const comment = [
      ...document.querySelectorAll<HTMLInputElement>(
        ".modal__panel input[type='text']",
      ),
    ][1];
    comment.value = "reviewed import";
    comment.dispatchEvent(new Event("input", { bubbles: true }));

    const importButton = [
      ...document.querySelectorAll<HTMLButtonElement>(".modal__footer button"),
    ].find((b) => b.textContent?.includes("Import selected"))!;
    importButton.click();
    await flushPromises();

    const importCall = fetchSpy.mock.calls.find(
      ([url]) => url === "/api/v1/admin/domain_blocks/import",
    )!;
    const form = (importCall[1] as RequestInit).body as FormData;
    const csv = await (form.get("file") as File).text();
    expect(csv).toContain(
      "spam.example,silence,false,false,reviewed import,Imported from source.example,false",
    );
    expect(csv).not.toContain("noise.example");
    expect(document.querySelector("[role='dialog']")).toBeNull();
    expect(wrapper.find(".blocks__import-summary").text()).toContain(
      "1 created",
    );
    wrapper.unmount();
  });
});
