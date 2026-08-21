import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const entry = fileURLToPath(new URL("./src/shiki-syntax.js", import.meta.url));
const outDir = fileURLToPath(new URL("../target/web-assets/vite", import.meta.url));

export default defineConfig({
  build: {
    emptyOutDir: true,
    minify: true,
    outDir,
    rollupOptions: {
      input: entry,
      output: {
        chunkFileNames: "assets/shiki/chunks/[name]-[hash].mjs",
        entryFileNames: "assets/shiki/index.mjs",
      },
    },
    chunkSizeWarningLimit: 1000,
    target: "es2022",
  },
});
