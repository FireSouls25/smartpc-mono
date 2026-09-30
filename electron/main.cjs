// Smart PC — Electron main process (Node side, privileged).
// Spawns the Rust sidecar (local backend) and shows its UI.
// The renderer NEVER gets raw Node APIs: only window.smartpc from preload.cjs.
const {
  app,
  BrowserWindow,
  dialog,
  ipcMain,
  nativeTheme,
  safeStorage,
} = require("electron");
const path = require("node:path");
const fs = require("node:fs");
const { spawn } = require("node:child_process");
const crypto = require("node:crypto");

const isDev = !app.isPackaged;
let sidecar = null;

function sidecarBin() {
  const name =
    process.platform === "win32" ? "smartpc-native.exe" : "smartpc-native";
  if (isDev) {
    return path.join(__dirname, "..", "src", "native", "target", "debug", name);
  }
  return path.join(process.resourcesPath, "bin", name);
}

// Supabase configuration handed to the sidecar. Two sources, in order:
// 1. the process environment (CI, `SUPABASE_URL=… npm run dev:electron`)
// 2. `supabase.json` next to the app / in the project root (packaged builds)
// The sidecar is the only process that needs these: the renderer never
// talks to Supabase, so no key is exposed to the page.
function supabaseEnv() {
  const out = {};
  for (const key of [
    "SUPABASE_URL",
    "SUPABASE_PROJECT_REF",
    "SUPABASE_ANON_KEY",
    "SUPABASE_PUBLISHABLE_KEY",
    "SUPABASE_SERVICE_ROLE_KEY",
  ]) {
    if (process.env[key]) out[key] = process.env[key];
  }
  const candidates = [
    path.join(__dirname, "..", "supabase.json"),
    path.join(app.getPath("userData"), "supabase.json"),
  ];
  for (const file of candidates) {
    if (Object.keys(out).length >= 2) break;
    try {
      const cfg = JSON.parse(fs.readFileSync(file, "utf8"));
      for (const [k, v] of Object.entries(cfg)) {
        if (typeof v === "string" && v && !out[k]) out[k] = v;
      }
    } catch {
      /* absent or malformed: env-only configuration is fine */
    }
  }
  return out;
}

// Starts the sidecar with a random per-launch token on an OS-assigned port.
// Resolves once the binary prints its READY line.
async function startSidecar() {
  const bin = sidecarBin();
  const token = crypto.randomBytes(32).toString("hex");
  const db = path.join(app.getPath("userData"), "smartpc.db");
  const child = spawn(bin, ["--port", "0", "--token", token, "--db", db], {
    stdio: ["ignore", "pipe", "inherit"],
    env: { ...process.env, ...supabaseEnv() },
  });
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("timed out waiting for READY")),
      20000,
    );
    let buf = "";
    child.stdout.on("data", (d) => {
      buf += d.toString();
      const m = buf.match(/READY port=(\d+)/);
      if (m) {
        clearTimeout(timer);
        resolve(`http://127.0.0.1:${m[1]}`);
      }
    });
    child.on("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      reject(new Error("sidecar exited with code " + code));
    });
  });
  process.env.SIDECAR_URL = url;
  process.env.SIDECAR_TOKEN = token;
  return child;
}

function createWindow() {
  // Cold-start flash color follows the OS theme (the app default is `auto`,
  // so this matches on fresh installs; an explicit user override may flash
  // briefly — the renderer owns the theme past this point).
  const dark = nativeTheme.shouldUseDarkColors;
  const win = new BrowserWindow({
    width: 1280,
    height: 860,
    minWidth: 1024,
    minHeight: 680,
    autoHideMenuBar: true,
    backgroundColor: dark ? "#131315" : "#e9e9ec",
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });

  if (isDev) {
    win.loadURL(process.env.VITE_DEV_URL || "http://localhost:5173");
  } else {
    win.loadFile(path.join(__dirname, "../dist/index.html"));
  }
  return win;
}

// System channels. Heavy local work already lives in the sidecar process;
// executor/mic/vault IPC lands here in later iterations.
ipcMain.handle("system:ping", () => ({
  ok: true,
  at: new Date().toISOString(),
}));

// Token vault: refresh tokens encrypted at rest with the OS keychain
// (DPAPI / Keychain / libsecret) instead of renderer localStorage.
// Stored as {key: base64(encrypted)} in userData; values capped so a
// compromised renderer cannot turn this into arbitrary file storage.
function vaultPath() {
  return path.join(app.getPath("userData"), "smartpc-vault.json");
}

function readVaultFile() {
  try {
    const parsed = JSON.parse(fs.readFileSync(vaultPath(), "utf8"));
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

function writeVaultFile(map) {
  fs.writeFileSync(vaultPath(), JSON.stringify(map), { mode: 0o600 });
}

const vaultOk = (key, value) =>
  typeof key === "string" &&
  key.length > 0 &&
  key.length <= 128 &&
  (value === undefined ||
    (typeof value === "string" && value.length <= 16 * 1024));

ipcMain.handle("vault:available", () => safeStorage.isEncryptionAvailable());

ipcMain.handle("vault:set", (_e, key, value) => {
  if (!safeStorage.isEncryptionAvailable() || !vaultOk(key, value)) {
    return { ok: false };
  }
  try {
    const map = readVaultFile();
    map[key] = safeStorage.encryptString(value).toString("base64");
    writeVaultFile(map);
    return { ok: true };
  } catch {
    return { ok: false };
  }
});

ipcMain.handle("vault:get", (_e, key) => {
  if (!safeStorage.isEncryptionAvailable() || !vaultOk(key)) {
    return { value: null };
  }
  const enc = readVaultFile()[key];
  if (typeof enc !== "string") return { value: null };
  try {
    return { value: safeStorage.decryptString(Buffer.from(enc, "base64")) };
  } catch {
    return { value: null };
  }
});

ipcMain.handle("vault:delete", (_e, key) => {
  if (!vaultOk(key)) return { ok: false };
  try {
    const map = readVaultFile();
    delete map[key];
    writeVaultFile(map);
    return { ok: true };
  } catch {
    return { ok: false };
  }
});

app.whenReady().then(async () => {
  try {
    sidecar = await startSidecar();
  } catch (err) {
    dialog.showErrorBox(
      "Smart PC",
      "Local backend failed to start:\n" +
        err.message +
        "\n\nThe app will open without local services.",
    );
  }
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("before-quit", () => {
  if (sidecar) sidecar.kill();
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
