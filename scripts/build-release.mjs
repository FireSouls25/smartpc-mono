// Release packaging: renderer bundle + Rust sidecar + Electron installer.
//
// electron-builder cannot run `cargo build`, and `main.cjs` looks for the
// sidecar at `process.resourcesPath/bin/<name>` once packaged. So this script
// does the two build steps and stages the binary where electron-builder's
// `extraResources` (electron-builder.yml) expects it:
//
//   1. vite build                 → dist/                (the renderer)
//   2. cargo build --release      → src/native/target/release/smartpc-native
//   3. copy it to                → release/staging/bin/ (→ resources/bin/)
//   4. electron-builder           → release/out/
//
//   node scripts/build-release.mjs                 # installers for this OS
//   node scripts/build-release.mjs --dir           # unpacked, for a smoke test
//   node scripts/build-release.mjs --target=portable
//
// Native build deps (cmake, a C++ compiler, libasound2-dev) are required for
// step 2 — the sidecar compiles whisper.cpp. See README → Build.
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

// 4. Package ------------------------------------------------------------------
const builderArgs = ["electron-builder"];
if (has("dir")) builderArgs.push("--dir");
else if (arg("target")) builderArgs.push(`--${arg("target")}`);
run("npx", builderArgs);

console.log(`\n[release] done → ${outDir}`);
console.log(
  "[release] note: the pi agent harness is NOT bundled — it needs a local " +
    "`pi` install plus pi-bridge/node_modules (232 MB). The app still starts; " +
    "pi-backed turns need `npm --prefix pi-bridge install` on the target machine.",
);
