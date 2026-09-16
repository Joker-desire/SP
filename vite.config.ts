import { defineConfig } from "vite";
import { resolve } from "node:path";
import { readFileSync } from "node:fs";
// @ts-expect-error type error without @types/node package
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// 版本号以 tauri.conf.json 为唯一来源，构建时注入前端——
// 关于窗口显示版本号不能依赖运行时 API（拿不到就只能空着）
const tauriConf = JSON.parse(
  readFileSync(resolve(process.cwd(), "src-tauri/tauri.conf.json"), "utf8")
);

// https://vite.dev/config/
export default defineConfig(() => ({

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
  // 主窗口之外还有「关于」窗口，多页打包必须把两个入口都列进来，
  // 否则 about.html 只在 dev server 下能打开，打包后就是 404
  define: {
    __APP_VERSION__: JSON.stringify(tauriConf.version ?? "0.0.0"),
  },
  build: {
    rollupOptions: {
      input: {
        main: resolve(process.cwd(), "index.html"),
        about: resolve(process.cwd(), "about.html"),
      },
    },
  },
}));
