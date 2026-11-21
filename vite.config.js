import { defineConfig } from "vite";

// Tauri expects a fixed dev port and serves dist/ in production.
export default defineConfig({
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { target: "es2021", outDir: "dist", emptyOutDir: true },
});
