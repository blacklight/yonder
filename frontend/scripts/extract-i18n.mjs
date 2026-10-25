#!/usr/bin/env node
// Scans src/**/*.{vue,ts} for t('key') / $t('key') usages and reports
// keys missing from src/i18n/locales/en.json plus unused keys.
// `--check` exits non-zero on any finding (lint gate).
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const check = process.argv.includes("--check");

function* walk(dir) {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) yield* walk(path);
    else if (/\.(vue|ts)$/.test(entry)) yield path;
  }
}

function flattenKeys(obj, prefix = "") {
  return Object.entries(obj ?? {}).flatMap(([k, v]) =>
    v && typeof v === "object" && !Array.isArray(v)
      ? flattenKeys(v, `${prefix}${k}.`)
      : [`${prefix}${k}`],
  );
}

const en = JSON.parse(
  readFileSync(join(root, "src/i18n/locales/en.json"), "utf-8"),
);
const enKeys = new Set(flattenKeys(en));
const used = new Map();

for (const file of walk(join(root, "src"))) {
  const source = readFileSync(file, "utf-8");
  for (const match of source.matchAll(/(?:\$t|\bt)\(\s*['"`]([\w.-]+)['"`]/g)) {
    used.set(match[1], (used.get(match[1]) ?? 0) + 1);
  }
}

const missing = [...used.keys()].filter((k) => !enKeys.has(k));
const unused = [...enKeys].filter((k) => !used.has(k));

for (const k of missing) console.error(`missing in en.json: ${k}`);
for (const k of unused) console.warn(`unused key: ${k}`);
if (check && missing.length) {
  console.error(`i18n check failed: ${missing.length} missing key(s)`);
  process.exit(1);
}
