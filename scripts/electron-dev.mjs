// Hardened dev runner: builds the Rust sidecar, starts Vite, opens Electron.
// The app opens in its OWN desktop window; no browser needed.
//
// Terminal hygiene (the Ctrl-keys-print-garbage issue): children run in their
// own process groups, every shutdown path kills the whole group AND restores
// the terminal (stty sane + cursor/attrs reset). If anything still wedges it:
//   npm run dev:clean   (kills strays from another shell)
//   reset               (re-inits the terminal; `clear` only repaints)
import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const shell = process.platform === "win32";
const HOST = "127.0.0.1";
const PORT = 5173;
const GROUP = process.platform !== "win32"; // own pgid → kill(-pid) hits the tree, never us

// `.env` is Vite's source for VITE_* (renderer) and, since the sidecar owns
// the Supabase keys, ours too: the sidecar reads its config from the process
// environment, and Electron forwards it to the spawned child.
try {
  const envFile = path.join(import.meta.dirname, "..", ".env");
  if (fs.existsSync(envFile)) process.loadEnvFile(envFile);
} catch {
  // Older Node, or a malformed .env: the shell environment still works.
}

console.log("[dev] building sidecar (cargo)…");
await new Promise((resolve, reject) => {
  const build = spawn(
    "cargo",
    ["build", "--manifest-path", "src/native/Cargo.toml"],
    { stdio: "inherit", shell },
  );
  build.on("exit", (code) =>
    code === 0 ? resolve() : reject(new Error("cargo build failed")),
  );
  build.on("error", reject);
});

const children = new Set();
function spawnTracked(file, args, opts) {
  const c = spawn(file, args, { ...opts, detached: GROUP });
  children.add(c);
  c.on("exit", () => children.delete(c));
  return c;
}

function killTree() {
  for (const c of [...children]) {
    try {
      if (c.exitCode === null && c.pid !== undefined) {
        if (GROUP) {
          try {
            process.kill(-c.pid, "SIGTERM"); // whole group, not us (we're detached apart)
            continue;
          } catch {
            /* fall through to single kill */
          }
        }
        c.kill("SIGTERM");
      }
    } catch {
      /* already gone */
    }
  }
}

function restoreTerminal() {
  try {
    process.stdout.write("\x1b[?25h\x1b[0m"); // show cursor, reset attrs
  } catch {
    /* non-tty */
  }
  if (process.platform !== "win32") {
    try {
      execSync("stty sane < /dev/tty", { stdio: "ignore", shell: "/bin/sh" });
    } catch {
      /* no tty (CI) — nothing to restore */
    }
  }
}

let shuttingDown = false;
function shutdown(code = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  killTree();
  restoreTerminal();
  process.exit(code);
}

// Fire-and-forget: spawnTracked owns the handle (see children set above).
spawnTracked(
  "node_modules/.bin/vite",
  ["--port", String(PORT), "--host", HOST, "--strictPort"],
  { stdio: "inherit", shell },
);

async function waitFor(url, tries = 60) {
  for (let i = 0; i < tries; i++) {
    try {
      await fetch(url);
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 500));
    }
  }
  throw new Error("vite did not start at " + url);
}

try {
  await waitFor(`http://${HOST}:${PORT}/`);
  console.log("[dev] vite ready → launching Electron (desktop window)…");
  const app = spawnTracked("node_modules/.bin/electron", ["."], {
    stdio: "inherit",
    shell,
    env: { ...process.env, VITE_DEV_URL: `http://${HOST}:${PORT}` },
  });
  app.on("exit", (code) => {
    if (code !== 0 && code !== null) {
      console.error(
        `[dev] electron exited with code ${code}. On Linux this usually means ` +
          `missing system libraries (libgtk-3-0, libnss3, libasound2, libxss1…).`,
      );
    }
    shutdown(code ?? 0);
  });
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(sig, () => {
      app.kill();
      shutdown(sig === "SIGINT" ? 0 : 1);
    });
  }
  process.on("uncaughtException", (err) => {
    console.error(err);
    shutdown(1);
  });
  process.on("exit", restoreTerminal);
} catch (err) {
  console.error(err);
  shutdown(1);
}
