import { ref } from "vue";
import { ApiError } from "@/api/errors";
import {
  downloadExport,
  downloadInlineExport,
  listExports,
  requestArchiveExport,
} from "@/api/settings";
import type { TransferRow } from "@/api/entities";

/** Standalone CSV/JSON export kinds — `filters` is Mastodon's JSON. */
export const EXPORT_KINDS = [
  "following",
  "blocking",
  "muting",
  "domain_blocks",
  "bookmarks",
  "lists",
  "filters",
] as const;

export function saveBlob(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
}

function errMessage(e: unknown): string {
  return e instanceof ApiError ? e.message : String(e);
}

/**
 * Export rows + inline downloads shared by the data settings and the
 * restricted (suspended-account) surface. `busy` holds the active
 * kind, `"archive"` for a queued request, or a row id for a download.
 */
export function useExports() {
  const rows = ref<TransferRow[]>([]);
  const loading = ref(true);
  const busy = ref<string | null>(null);
  const error = ref("");
  /** Which section produced `error` — pages show it in place. */
  const errorArea = ref<"csv" | "archive" | null>(null);

  function fail(e: unknown, area: "csv" | "archive") {
    error.value = errMessage(e);
    errorArea.value = area;
  }

  async function refresh() {
    try {
      rows.value = await listExports();
    } catch (e) {
      fail(e, "archive");
    } finally {
      loading.value = false;
    }
  }

  async function requestArchive() {
    error.value = "";
    errorArea.value = null;
    busy.value = "archive";
    try {
      const row = await requestArchiveExport();
      rows.value = [row, ...rows.value];
    } catch (e) {
      fail(e, "archive");
    } finally {
      busy.value = null;
    }
  }

  async function downloadArchive(row: TransferRow) {
    error.value = "";
    errorArea.value = null;
    busy.value = row.id;
    try {
      const { blob, filename } = await downloadExport(row.id);
      saveBlob(blob, filename || "archive.zip");
    } catch (e) {
      fail(e, "archive");
    } finally {
      busy.value = null;
    }
  }

  async function downloadKind(kind: string) {
    error.value = "";
    errorArea.value = null;
    busy.value = kind;
    try {
      const { blob, filename } = await downloadInlineExport(kind);
      saveBlob(blob, filename || `${kind}.csv`);
    } catch (e) {
      fail(e, "csv");
    } finally {
      busy.value = null;
    }
  }

  return {
    rows,
    loading,
    busy,
    error,
    errorArea,
    refresh,
    requestArchive,
    downloadArchive,
    downloadKind,
  };
}
