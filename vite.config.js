import { defineConfig } from "vite";

// Tauri expects a fixed dev port and serves dist/ in production.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    // Rust build output and sources: watching them crashes Vite on Windows (locked DLLs during the build).
    watch: { ignored: ["**/target/**", "**/src-tauri/**", "**/crates/**"] },
  },
  build: { target: "es2021", outDir: "dist", emptyOutDir: true },
});
