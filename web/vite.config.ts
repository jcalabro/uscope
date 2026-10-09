import react from "@vitejs/plugin-react";
import { playwright } from "@vitest/browser-playwright";
import { defineConfig } from "vitest/config";

// `just web-dev` runs `uscope web` on this port and lets it accept pages
// from the development server, which proxies the API to it.
const server = `127.0.0.1:${process.env.USCOPE_WEB_PORT ?? "7342"}`;

export default defineConfig({
  plugins: [react()],
  // Files are named relative to the page, so it can be served under any
  // path; the server roots them at its own (`assets::rooted`).
  base: "./",
  build: {
    outDir: "../build/web",
    emptyOutDir: true,
    sourcemap: true,
    // Inlined assets are data: URLs, which the page's CSP refuses.
    assetsInlineLimit: 0,
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    proxy: {
      "/api/ws": { target: `ws://${server}`, ws: true, changeOrigin: true },
      "/api": { target: `http://${server}`, changeOrigin: true },
    },
  },
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: "unit",
          include: ["test/**/*.test.ts"],
          environment: "node",
        },
      },
      {
        extends: true,
        test: {
          name: "browser",
          include: ["test/**/*.test.tsx"],
          browser: {
            enabled: true,
            headless: true,
            provider: playwright(),
            instances: [{ browser: "chromium" }],
            screenshotFailures: false,
          },
        },
      },
    ],
  },
});
