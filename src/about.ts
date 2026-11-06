// 关于窗口：版本号在构建时由 vite 注入（__APP_VERSION__，读自 tauri.conf.json），
// 不依赖运行时 API——那些 API 在预览环境拿不到，版本行会空着。
declare const __APP_VERSION__: string;

const el = document.getElementById("version");
if (el) {
  el.textContent = `Version ${__APP_VERSION__}`;
}

// 藏起来的致谢：按住 ⌥（Alt）才现身，松开还原。
// 失焦时也还原，免得切走再回来一直挂着。
const cn = document.querySelector<HTMLElement>(".slogan .cn");
const normal = cn?.textContent ?? "";
const secret = "献给每一个舍不得删片的人。";
window.addEventListener("keydown", (e) => {
  if (e.altKey && cn) cn.textContent = secret;
});
window.addEventListener("keyup", (e) => {
  if (!e.altKey && cn) cn.textContent = normal;
});
window.addEventListener("blur", () => {
  if (cn) cn.textContent = normal;
});
