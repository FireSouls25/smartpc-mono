import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [svelte(), tailwindcss()],
  server: { port: 5173 },
  // Relative asset URLs: the packaged app loads dist/index.html over
  // file:// (win.loadFile), where absolute /assets/... paths resolve to
  // file:///assets/... and 404 — leaving #app empty (black window).
  base: "./",
});
