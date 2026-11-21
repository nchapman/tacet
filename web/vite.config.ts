import { writeFileSync } from "node:fs";
import path from "node:path";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

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
  plugins: [react(), preserveGitkeep()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8080",
        changeOrigin: true,
      },
      "/media": {
        target: "http://127.0.0.1:8080",
        changeOrigin: true,
      },
      "/upnp": {
        target: "http://127.0.0.1:8080",
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
