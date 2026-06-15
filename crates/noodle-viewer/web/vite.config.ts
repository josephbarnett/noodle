import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// During `npm run dev`, the Vite dev server runs at :5173 and proxies
// /ws + /api/* to the Rust backend at :9092 so the live data path
// works without giving up hot reload.
//
// In production, the Rust binary serves the built `dist/` directly
// via rust-embed; this config doesn't run.
export default defineConfig({
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:9092",
      "/ws": {
        target: "ws://127.0.0.1:9092",
        ws: true,
      },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: true,
  },
  test: {
    environment: "jsdom",
  },
});
