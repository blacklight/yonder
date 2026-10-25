import { request } from "./client";
import type { CustomEmoji } from "./entities";

/** Admin emoji entity — `CustomEmoji` plus management fields. */
export interface AdminEmoji extends CustomEmoji {
  id: string;
  disabled: boolean;
  pack_id: string | null;
}

export interface EmojiPack {
  id: string;
  name: string;
  emoji_count?: number;
}

/** `{imported, skipped, results[]}` shared by both import endpoints. */
export interface ImportReport {
  imported: number;
  skipped: number;
  results: { name: string; ok: boolean; error?: string }[];
}

export function listAdminEmojis(
  params: {
    pack_id?: string;
    include_disabled?: boolean;
  } = {},
): Promise<AdminEmoji[]> {
  return request({ path: "/admin/emojis", query: { ...params } });
}

/** `POST /api/v1/admin/emojis` — multipart `file` + `shortcode`. */
export function uploadEmoji(body: {
  file: File;
  shortcode: string;
  pack_id?: string;
  category?: string;
}): Promise<AdminEmoji> {
  const form = new FormData();
  form.set("file", body.file);
  form.set("shortcode", body.shortcode);
  if (body.pack_id) form.set("pack_id", body.pack_id);
  if (body.category) form.set("category", body.category);
  return request({ path: "/admin/emojis", method: "POST", body: form });
}

/** `PATCH /api/v1/admin/emojis/{id}` — tri-state fields. */
export function patchEmoji(
  id: string,
  body: {
    shortcode?: string;
    category?: string | null;
    visible_in_picker?: boolean;
    disabled?: boolean;
    pack_id?: string | null;
  },
): Promise<AdminEmoji> {
  return request({
    path: `/admin/emojis/${encodeURIComponent(id)}`,
    method: "PATCH",
    body,
  });
}

export function deleteEmoji(id: string): Promise<object> {
  return request({
    path: `/admin/emojis/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

/** `POST …/import_zip` — multipart zip of images. */
export function importEmojiZip(
  file: File,
  pack_id?: string,
): Promise<ImportReport> {
  const form = new FormData();
  form.set("file", file);
  if (pack_id) form.set("pack_id", pack_id);
  return request({
    path: "/admin/emojis/import_zip",
    method: "POST",
    body: form,
  });
}

/** `POST …/import_dir` — flat scan under `admin.emoji_import_dir`. */
export function importEmojiDir(
  from_dir: string,
  pack_id?: string,
): Promise<ImportReport> {
  return request({
    path: "/admin/emojis/import_dir",
    method: "POST",
    body: { from_dir, ...(pack_id ? { pack_id } : {}) },
  });
}

export function listEmojiPacks(): Promise<EmojiPack[]> {
  return request({ path: "/admin/emoji_packs" });
}

export function createEmojiPack(name: string): Promise<EmojiPack> {
  return request({
    path: "/admin/emoji_packs",
    method: "POST",
    body: { name },
  });
}

export function renameEmojiPack(id: string, name: string): Promise<EmojiPack> {
  return request({
    path: `/admin/emoji_packs/${encodeURIComponent(id)}`,
    method: "PATCH",
    body: { name },
  });
}

export function deleteEmojiPack(id: string): Promise<object> {
  return request({
    path: `/admin/emoji_packs/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}
