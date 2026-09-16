// 关于窗口：版本号在构建时由 vite 注入（__APP_VERSION__，读自 tauri.conf.json），
// 不依赖运行时 API——那些 API 在预览环境拿不到，版本行会空着。
declare const __APP_VERSION__: string;

const el = document.getElementById("version");
if (el) {
  el.textContent = `Version ${__APP_VERSION__}`;
}
