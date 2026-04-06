import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import { fileURLToPath, URL } from "node:url";

export default defineConfig({
  base: "/admin/assets/",
  plugins: [vue()],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  build: {
    assetsDir: "",
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (id.includes("/node_modules/vue/")) {
            return "vue";
          }
          if (id.includes("/node_modules/echarts/")) {
            return "echarts";
          }
          return undefined;
        },
      },
    },
  },
  server: {
    host: "0.0.0.0",
    port: 5173,
    strictPort: true,
    proxy: {
      "/admin/api": {
        target: "http://127.0.0.1:8765",
        changeOrigin: true,
      },
      "/admin/assets": {
        target: "http://127.0.0.1:8765",
        changeOrigin: true,
      },
      "/admin": {
        target: "http://127.0.0.1:8765",
        changeOrigin: true,
      },
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: [],
  },
});
