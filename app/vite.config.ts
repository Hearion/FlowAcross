import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 开发模式固定使用 5173 端口（见 src-tauri/tauri.conf.json 的 devUrl）。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "es2022",
    outDir: "dist",
  },
});
