// Release packaging: renderer bundle + Rust sidecar + vendored pi + installer.
//
// electron-builder cannot run `cargo build`, and `main.cjs` looks for the
// sidecar at `process.resourcesPath/bin/<name>` once packaged. So this script
// does the build steps and stages artifacts where electron-builder's
// `extraResources` (electron-builder.yml) expects them:
//
//   1. vite build                 → dist/                     (the renderer)
//   2. cargo build --release      → src/native/target/release/smartpc-native
//   3. copy it to                → release/staging/bin/      (→ resources/bin/)
//   3b. copy pi-bridge (+prune)  → release/staging/pi-bridge (→ resources/)
//   4. electron-builder           → release/out/
//
//   node scripts/build-release.mjs                 # installers for this OS
//   node scripts/build-release.mjs --dir           # unpacked, for a smoke test
//   node scripts/build-release.mjs --target=portable
//
// Native build deps (cmake, a C++ compiler, libasound2-dev) are required for
// step 2 — the sidecar compiles whisper.cpp. See README → Build.
// Step 3b needs `npm --prefix pi-bridge ci` first (CI does it explicitly).
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "..");
const nativeDir = path.join(root, "src", "native");
const staging = path.join(root, "release", "staging");
const outDir = path.join(root, "release", "out");

const argv = process.argv.slice(2);
const arg = (name) =>
  argv.find((a) => a.startsWith(`--${name}=`))?.split("=")[1];
const has = (name) => argv.includes(`--${name}`);

const shell = process.platform === "win32";

function run(cmd, args) {
  console.log(`\n[release] ${cmd} ${args.join(" ")}`);
  const res = spawnSync(cmd, args, { stdio: "inherit", shell, cwd: root });
  if (res.status !== 0) {
    console.error(`[release] failed: ${cmd} exited ${res.status}`);
    process.exit(res.status ?? 1);
  }
}

// 1. Renderer -----------------------------------------------------------------
run("npx", ["vite", "build"]);

// 2. Sidecar ------------------------------------------------------------------
run("cargo", [
  "build",
  "--release",
  "--manifest-path",
  path.join(nativeDir, "Cargo.toml"),
]);

// 3. Stage the sidecar where electron-builder.yml picks it up ---------------
const binName =
  process.platform === "win32" ? "smartpc-native.exe" : "smartpc-native";
const builtBin = path.join(nativeDir, "target", "release", binName);
if (!fs.existsSync(builtBin)) {
  console.error(`[release] sidecar binary missing at ${builtBin}`);
  process.exit(1);
}
fs.rmSync(staging, { recursive: true, force: true });
fs.mkdirSync(path.join(staging, "bin"), { recursive: true });
fs.copyFileSync(builtBin, path.join(staging, "bin", binName));
// Executable bit: a copy made from a fresh build keeps it, but asar-less
// resources on some filesystems do not. Cheap insurance.
try {
  fs.chmodSync(path.join(staging, "bin", binName), 0o755);
} catch {
  /* Windows has no executable bit */
}
const mb = (fs.statSync(builtBin).size / 1024 / 1024).toFixed(1);
console.log(`[release] staged bin/${binName} (${mb} MB)`);

// 3b. Stage pi-bridge (vendored pi CLI + tool bridge + TTS daemon) ---------
// The sidecar self-resolves it under resources/pi-bridge (packaged) or the
// dev tree, and spawns the vendored CLI via the local node — no npx, no
// network, no conflict with the user's own pi. The copy is pruned for the
// host platform (esbuild ships ~20 platform binaries; docs/examples and
// @types never run) to keep installers lean.
function dirSize(p) {
  let total = 0;
  for (const e of fs.readdirSync(p, { withFileTypes: true })) {
    const full = path.join(p, e.name);
    if (e.isDirectory()) total += dirSize(full);
    else if (e.isFile()) total += fs.statSync(full).size;
  }
  return total;
}
function stagePiBridge() {
  const src = path.join(root, "pi-bridge");
  const nm = path.join(src, "node_modules");
  if (!fs.existsSync(path.join(nm, "@earendil-works", "pi-coding-agent"))) {
    console.error(
      "[release] vendored pi missing: run `npm --prefix pi-bridge ci` first",
    );
    process.exit(1);
  }
  // NOTE: deps land in pi-bridge-deps/, NOT pi-bridge/node_modules:
  // electron-builder hard-drops a root `node_modules` under extraResources
  // `from` (see app-builder-lib filter.js), but passes nested trees through.
  // The `to:` side restores the runtime layout (resources/pi-bridge/…).
  const dest = path.join(staging, "pi-bridge");
  const destDeps = path.join(staging, "pi-bridge-deps");
  fs.rmSync(dest, { recursive: true, force: true });
  fs.rmSync(destDeps, { recursive: true, force: true });
  fs.mkdirSync(dest, { recursive: true });
  fs.mkdirSync(destDeps, { recursive: true });
  for (const name of ["smartpc.ts", "tts-synth.js", "package.json"]) {
    fs.copyFileSync(path.join(src, name), path.join(dest, name));
  }
  for (const e of fs.readdirSync(nm, { withFileTypes: true })) {
    if (!e.isDirectory()) continue;
    if (e.name.startsWith("@types") || e.name === ".package-lock.json")
      continue;
    fs.cpSync(path.join(nm, e.name), path.join(destDeps, e.name), {
      recursive: true,
    });
  }
  // Prune (recursive — the bulk hides in nested trees, e.g. esbuild's ~20
  // platform binaries inside the pi package; pi compiles the .ts bridge
  // at runtime so it needs exactly the host's).
  const esHost =
    { linux: "linux", darwin: "darwin", win32: "win32" }[process.platform] +
    "-" +
    { x64: "x64", arm64: "arm64" }[process.arch];
  let pruned = 0;
  function pruneTree(dir, inPiPkg) {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      const full = path.join(dir, e.name);
      const piPkg = inPiPkg || e.name === "pi-coding-agent";
      if (e.name === "@esbuild") {
        for (const plat of fs.readdirSync(full)) {
          if (plat !== esHost) {
            pruned += dirSize(path.join(full, plat));
            fs.rmSync(path.join(full, plat), { recursive: true, force: true });
          }
        }
      } else if (piPkg && (e.name === "docs" || e.name === "examples")) {
        pruned += dirSize(full);
        fs.rmSync(full, { recursive: true, force: true });
        continue;
      } else if (e.name === "@types") {
        pruned += dirSize(full);
        fs.rmSync(full, { recursive: true, force: true });
        continue;
      }
      pruneTree(full, piPkg);
    }
  }
  pruneTree(destDeps, false);
  const staged =
    dirSize(destDeps) +
    fs.statSync(path.join(dest, "smartpc.ts")).size +
    fs.statSync(path.join(dest, "tts-synth.js")).size +
    fs.statSync(path.join(dest, "package.json")).size;
  console.log(
    `[release] staged pi-bridge (${(staged / 1024 / 1024).toFixed(1)} MB, pruned ${(pruned / 1024 / 1024).toFixed(1)} MB, esbuild:${esHost})`,
  );
}
stagePiBridge();

// 4. Package ------------------------------------------------------------------
const builderArgs = ["electron-builder", "--publish", "never"];
if (has("dir")) builderArgs.push("--dir");
else if (arg("target")) builderArgs.push(`--${arg("target")}`);
run("npx", builderArgs);

console.log(`\n[release] done → ${outDir}`);
console.log(
  "[release] note: pi CLI + bridge + TTS daemon ship under resources/pi-bridge " +
    "(own PI_CODING_AGENT_DIR, no conflict with the user's pi). A node runtime " +
    "is still required on the target machine (pi CLI + tts-synth spawn via node).",
);
