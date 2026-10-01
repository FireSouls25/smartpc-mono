// Regenerates src/native/assets/pi-models.json from pi's built-in model
// registry: the static fallback behind the provider catalog for ids pi's
// live `get_available_models` RPC doesn't report (it only lists
// authenticated + local providers).
//
// The registry ships inside pi's npm package; point at its data dir:
//   node scripts/regen-pi-models.mjs --pi-data-dir <...>/pi-ai/src/providers/data
//   node scripts/regen-pi-models.mjs --pi-package-dir <...>/@earendil-works  (resolves data dir itself)
//
// Output: { pi_version, generated_at, providers: { id: [model ids...] } }.
// Bump `pi_version` by hand to the pi release you extracted from
// (see PI_PACKAGE in src/native/src/pi/mod.rs). Model ids go stale as pi
// revs its catalog; the live RPC always wins at runtime, so staleness only
// affects never-authenticated providers until their key is saved.
import { readdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import path from "node:path";

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? null : process.argv[i + 1];
}

let dataDir = arg("pi-data-dir");
const pkgDir = arg("pi-package-dir");
if (!dataDir && pkgDir) {
  dataDir = path.join(pkgDir, "pi-ai", "src", "providers", "data");
}
if (!dataDir || !existsSync(dataDir)) {
  console.error(
    "usage: node scripts/regen-pi-models.mjs --pi-data-dir <dir> | --pi-package-dir <@earendil-works dir>",
  );
  process.exit(1);
}

const providers = {};
for (const file of readdirSync(dataDir).sort()) {
  if (!file.endsWith(".json") || file.startsWith(".")) continue;
  const id = file.slice(0, -".json".length);
  let doc;
  try {
    doc = JSON.parse(readFileSync(path.join(dataDir, file), "utf8"));
  } catch (e) {
    console.error(`skip ${file}: ${e.message}`);
    continue;
  }
  const ids = new Set();
  const walk = (o) => {
    if (Array.isArray(o)) {
      for (const v of o) walk(v);
    } else if (o && typeof o === "object") {
      if (typeof o.id === "string" && typeof o.provider === "string") {
        ids.add(o.id);
      }
      for (const v of Object.values(o)) walk(v);
    }
  };
  walk(doc);
  // Drop OAuth-only/subscription companions with no key story? No — keep
  // everything pi ships; paste-gating lives in Rust (accepts_pasted_key).
  if (ids.size > 0) providers[id] = [...ids].sort();
}

const out = {
  pi_version: "0.85.1",
  generated_at: new Date().toISOString(),
  source: "pi packages/ai provider registry (providers/data/*.json)",
  providers,
};
const dest = path.join(
  import.meta.dirname,
  "..",
  "src",
  "native",
  "assets",
  "pi-models.json",
);
writeFileSync(dest, JSON.stringify(out, null, 1) + "\n");
const ids = Object.keys(providers).length;
const models = Object.values(providers).reduce((n, l) => n + l.length, 0);
console.log(`wrote ${dest}: ${ids} providers, ${models} models`);
