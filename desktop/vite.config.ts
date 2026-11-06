import { defineConfig } from "vite";

// Tauri expects a fixed port; it must not fall back to another one.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "safari15",
    sourcemap: false,
    reportCompressedSize: false,
  },
});
