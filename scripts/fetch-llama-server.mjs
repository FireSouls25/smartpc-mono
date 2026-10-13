// Fetch the pinned llama-server build for the host OS and stage it into
// release/staging/bin/ (resources/bin/ once packaged).
//
// Source of truth mirrors src/native/src/ai/local.rs (LLAMA_BUILD + asset
// table): bump both together. sha256 comes from the GitHub release API.
// Extraction uses only OS tools (tar on unix, Expand-Archive on Windows).
import { spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const BUILD = "b11429";
const GH = `https://github.com/ggml-org/llama.cpp/releases/download/${BUILD}`;

const ASSETS = {
  "linux-x64": {
    file: `llama-${BUILD}-bin-ubuntu-x64.tar.gz`,
    sha256: "f6d25dde8f51133143d1453da4fd5f73b145127177612a283bf7995957af3392",
    kind: "tar.gz",
  },
  "darwin-arm64": {
    file: `llama-${BUILD}-bin-macos-arm64.tar.gz`,
    sha256: "740288ec6887be94280a5dfa25b5e23a78285cab104519e6c7e218904ee82459",
    kind: "tar.gz",
  },
  "win32-x64": {
    file: `llama-${BUILD}-bin-win-cpu-x64.zip`,
    sha256: "1283323272b04cd07905816a597a0da810918102de958f4ff6f7bbaa70ed2efe",
    kind: "zip",
  },
};

const root = path.resolve(import.meta.dirname, "..");
const staging = path.join(root, "release", "staging");

function sh(cmd, args) {
  const res = spawnSync(cmd, args, { stdio: "inherit", cwd: root });
  if (res.status !== 0) {
    console.error(`[llama-server] failed: ${cmd} exited ${res.status}`);
    process.exit(res.status ?? 1);
  }
}

function walk(dir, want, depth, out) {
  if (depth === 0) return;
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) {
      if (!e.name.startsWith(".")) walk(full, want, depth - 1, out);
    } else if (e.name === want) {
      out.push(full);
    }
  }
}

const key = `${process.platform}-${process.arch}`;
const asset = ASSETS[key];
if (!asset) {
  console.error(
    `[llama-server] no pinned build for ${key} — set LLAMACPP_SERVER_BIN or add an asset (see src/native/src/ai/local.rs)`,
  );
  process.exit(1);
}

fs.mkdirSync(path.join(staging, "bin"), { recursive: true });
const binName =
  process.platform === "win32" ? "llama-server.exe" : "llama-server";
const marker = path.join(staging, "bin", ".llama-build");
if (
  fs.existsSync(path.join(staging, "bin", binName)) &&
  fs.existsSync(marker) &&
  fs.readFileSync(marker, "utf8").trim() === BUILD
) {
  console.log(`[llama-server] already staged (${binName}, build ${BUILD})`);
  process.exit(0);
}

const url = `${GH}/${asset.file}`;
const archive = path.join(staging, asset.file);
console.log(`[llama-server] downloading ${url}`);
const res = await fetch(url);
if (!res.ok) {
  console.error(`[llama-server] download failed: HTTP ${res.status}`);
  process.exit(1);
}
const hash = crypto.createHash("sha256");
const out = fs.createWriteStream(archive);
for await (const chunk of res.body) {
  hash.update(chunk);
  out.write(chunk);
}
out.end();
await new Promise((resolve, reject) => {
  out.on("finish", resolve);
  out.on("error", reject);
});
const digest = hash.digest("hex");
if (digest !== asset.sha256) {
  fs.rmSync(archive, { force: true });
  console.error("[llama-server] checksum mismatch — deleted, try again");
  process.exit(1);
}

const distDir = path.join(staging, "llama-dist");
fs.rmSync(distDir, { recursive: true, force: true });
fs.mkdirSync(distDir, { recursive: true });
if (asset.kind === "tar.gz") {
  sh("tar", ["xzf", archive, "-C", distDir]);
} else {
  sh("powershell", [
    "-NoProfile",
    "-Command",
    `Expand-Archive -Path '${archive}' -DestinationPath '${distDir}' -Force`,
  ]);
}

const found = [];
walk(distDir, binName, 5, found);
found.sort((a, b) => a.length - b.length);
if (found.length === 0) {
  console.error(`[llama-server] ${binName} not found in ${asset.file}`);
  process.exit(1);
}
const serverBin = found[0];
fs.copyFileSync(serverBin, path.join(staging, "bin", binName));
// Sibling shared libs ride along (newer llama.cpp ships llama-server as a
// small stub over libllama-*.so). Symlinks included: sonames like
// libllama-common.so.0 point at the versioned files.
const sibDir = path.dirname(serverBin);
for (const e of fs.readdirSync(sibDir, { withFileTypes: true })) {
  if (e.name === binName) continue;
  const src = path.join(sibDir, e.name);
  const dest = path.join(staging, "bin", e.name);
  if (e.isSymbolicLink()) {
    fs.symlinkSync(fs.readlinkSync(src), dest);
    continue;
  }
  if (
    e.isFile() &&
    (e.name.endsWith(".so") ||
      e.name.includes(".so.") ||
      e.name.endsWith(".dylib") ||
      e.name.endsWith(".dll"))
  ) {
    fs.copyFileSync(src, dest);
  }
}
if (process.platform !== "win32") {
  fs.chmodSync(path.join(staging, "bin", binName), 0o755);
}
fs.rmSync(archive, { force: true });
fs.rmSync(distDir, { recursive: true, force: true });
fs.writeFileSync(marker, `${BUILD}\n`);
const mb = (
  fs.statSync(path.join(staging, "bin", binName)).size /
  1024 /
  1024
).toFixed(1);
console.log(`[llama-server] staged bin/${binName} (${mb} MB, build ${BUILD})`);
