import js from "@eslint/js";
import tseslint from "typescript-eslint";
import svelte from "eslint-plugin-svelte";
import prettier from "eslint-config-prettier";
import globals from "globals";

export default [
  {
    ignores: [
      "dist/**",
      "test-results/**",
      "src/native/target/**",
      "lancedb/**",
    ],
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  ...svelte.configs["flat/recommended"],
  // The svelte parser delegates <script> blocks (and .svelte.ts modules)
  // to this sub-parser: without it, TypeScript syntax fails to parse.
  {
    files: ["**/*.svelte", "**/*.svelte.js", "**/*.svelte.ts"],
    languageOptions: {
      parserOptions: {
        parser: tseslint.parser,
      },
    },
  },
  // Prettier owns formatting: disable conflicting stylistic rules.
  prettier,
  {
    languageOptions: {
      globals: { ...globals.browser, ...globals.node },
    },
  },
  {
    // Electron main/preload are CommonJS by platform requirement
    // (Electron loads .cjs as CJS regardless of package type).
    files: ["electron/**/*.cjs"],
    rules: {
      "@typescript-eslint/no-require-imports": "off",
    },
  },
  {
    // The TTS synth daemon (+ its tests) runs as plain node scripts
    // spawned directly by the sidecar — no bundler, so require() stays.
    files: ["pi-bridge/tts-synth.js", "pi-bridge/tts-synth.test.js"],
    rules: {
      "@typescript-eslint/no-require-imports": "off",
    },
  },
];
