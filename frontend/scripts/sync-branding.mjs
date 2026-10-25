#!/usr/bin/env node
// Copies branding assets from ../branding into public/ so the PWA
// manifest and site chrome stay in lockstep with the repo's source
// of truth. Run manually when branding assets change.
import { copyFileSync, mkdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const frontend = join(dirname(fileURLToPath(import.meta.url)), "..");
const branding = join(frontend, "..", "branding");

const copies = [
  ["logo-512.png", "public/icons/logo-512.png"],
  ["maskable-192.png", "public/icons/maskable-192.png"],
  ["maskable-512.png", "public/icons/maskable-512.png"],
  ["favicon.svg", "public/favicon.svg"],
  ["favicon.ico", "public/favicon.ico"],
  ["logo.svg", "public/logo.svg"],
];

for (const [from, to] of copies) {
  const src = join(branding, from);
  const dest = join(frontend, to);
  if (!existsSync(src)) {
    console.error(`missing branding asset: ${src}`);
    process.exit(1);
  }
  mkdirSync(dirname(dest), { recursive: true });
  copyFileSync(src, dest);
  console.log(`${from} → ${to}`);
}
