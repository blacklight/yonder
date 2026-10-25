#!/usr/bin/env node
// Regenerates src/api/types.ts from the backend's OpenAPI document.
//
// utoipa defaults `operationId` to the handler function name, so
// generic names (`show`, `list`, `update`, …) collide across modules.
// Left as-is, openapi-typescript emits an `operations` interface with
// duplicate identifiers that fails `vue-tsc`. This script suffixes
// repeated ids (`show`, `show_2`, `show_3`, …) before generation —
// path-based types (what the client uses) are unaffected.
//
// Usage: `npm run api:gen` (produces ../target/openapi.json first).
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";

const specPath = process.argv[2] ?? "../target/openapi.json";
const doc = JSON.parse(readFileSync(specPath, "utf-8"));

const seen = new Map();
let renamed = 0;
for (const item of Object.values(doc.paths ?? {})) {
  for (const op of Object.values(item)) {
    const id = op?.operationId;
    if (!id) continue;
    const n = (seen.get(id) ?? 0) + 1;
    seen.set(id, n);
    if (n > 1) {
      op.operationId = `${id}_${n}`;
      renamed += 1;
    }
  }
}
if (renamed) {
  writeFileSync(specPath, JSON.stringify(doc));
  console.log(`de-duplicated ${renamed} operationIds`);
}

const root = new URL("..", import.meta.url).pathname;
execFileSync(
  "npx",
  ["openapi-typescript", specPath, "-o", "src/api/types.ts"],
  { cwd: root, stdio: "inherit" },
);
execFileSync("npx", ["prettier", "--write", "src/api/types.ts"], {
  cwd: root,
  stdio: "inherit",
});
