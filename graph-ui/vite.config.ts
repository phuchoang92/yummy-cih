import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";

export default defineConfig({
  plugins: [react()],
  base: "/graph/assets/",
  build: {
    outDir: resolve(__dirname, "../crates/cih-server/assets/graph"),
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 1_500,
    rollupOptions: {
      output: {
        entryFileNames: "app.js",
        chunkFileNames: "chunk-[name].js",
        assetFileNames: (asset) => asset.name?.endsWith(".css") ? "styles.css" : "[name][extname]",
      },
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: "./src/test-setup.ts",
    exclude: ["e2e/**", "node_modules/**"],
  },
});
