import { writeFileSync } from "node:fs";
import path from "node:path";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";

const outDir = path.resolve(import.meta.dirname, "../internal/api/web/dist");

function preserveGitkeep(): Plugin {
  return {
    name: "preserve-gitkeep",
    closeBundle() {
      writeFileSync(path.join(outDir, ".gitkeep"), "");
    },
  };
}

export default defineConfig({
  plugins: [react(), tailwindcss(), preserveGitkeep()],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "src"),
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:4320",
        changeOrigin: true,
      },
      "/media": {
        target: "http://127.0.0.1:4320",
        changeOrigin: true,
      },
      "/upnp": {
        target: "http://127.0.0.1:4320",
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir,
    emptyOutDir: true,
    sourcemap: false,
  },
});
