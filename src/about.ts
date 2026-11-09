// 关于窗口：版本号在构建时由 vite 注入（__APP_VERSION__，读自 tauri.conf.json），
// 不依赖运行时 API——那些 API 在预览环境拿不到，版本行会空着。
//
// 「检查更新」按钮也在这里。这个窗口是独立的 Webview，updater 的 check() 和
// 更新面板都归主窗口管，所以只做两件事：发一个检查请求、等主窗口把结果送回来。
// 好处是 about 窗口只需要 core:default 权限，拿不到 updater / dialog 插件能力。
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";

declare const __APP_VERSION__: string;

const REQUEST = "sp://update-check-request";
const RESULT = "sp://update-check-result";

interface UpdateCheckResult {
  kind: "latest" | "available" | "error" | "busy";
  version?: string;
  message?: string;
}

const versionEl = document.getElementById("version");
if (versionEl) {
  versionEl.textContent = `Version ${__APP_VERSION__}`;
}

const btn = document.getElementById("check-update") as HTMLButtonElement | null;
const btnText = document.getElementById("check-update-text");
const status = document.getElementById("update-status");
const dialog = document.getElementById("update-dialog");
const dialogTitle = document.getElementById("update-dialog-title");
const dialogText = document.getElementById("update-dialog-text");
const dialogOk = document.getElementById("update-dialog-ok") as HTMLButtonElement | null;

let busy = false;
let timeoutId: number | undefined;

function setStatus(text: string) {
  if (status) status.textContent = text;
}

function setBusy(on: boolean) {
  busy = on;
  if (btn) btn.disabled = on;
  if (btnText) btnText.textContent = on ? "正在检查…" : "检查更新";
}

function finish() {
  if (timeoutId !== undefined) {
    window.clearTimeout(timeoutId);
    timeoutId = undefined;
  }
  setBusy(false);
}

function showDialog(title: string, text: string) {
  if (dialogTitle) dialogTitle.textContent = title;
  if (dialogText) dialogText.textContent = text;
  if (dialog) dialog.hidden = false;
  dialogOk?.focus();
}

function closeDialog() {
  if (!dialog || dialog.hidden) return;
  dialog.hidden = true;
  btn?.focus();
}

dialogOk?.addEventListener("click", closeDialog);
dialog?.addEventListener("click", (e) => {
  if (e.target === dialog) closeDialog();
});
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeDialog();
});

// 主窗口没回应（没启动完、崩溃、事件丢包）时不能把按钮一直锁着。
// 20 秒比主窗口 check() 的 15 秒超时略长，留出事件往返余量。
btn?.addEventListener("click", () => {
  if (busy) return;
  setBusy(true);
  setStatus("");
  timeoutId = window.setTimeout(() => {
    if (!busy) return;
    finish();
    setStatus("");
    showDialog("检查更新", "这次检查没有回应，稍后再试；也可以到 Release 页手动下载。");
  }, 20000);
  void emit(REQUEST).catch(() => {
    finish();
    showDialog("检查更新失败", "没能连上主窗口，稍后再试；也可以到 Release 页手动下载。");
  });
});

void listen<UpdateCheckResult>(RESULT, (event) => {
  // 只处理自己发出的那次请求；启动 8 秒的静默检查不往这里送结果。
  if (!busy) return;
  finish();
  const r = event.payload;
  switch (r.kind) {
    case "latest":
      setStatus("");
      showDialog("已是最新版本", `当前版本 ${__APP_VERSION__}，不需要更新。`);
      break;
    case "available":
      setStatus(`发现新版本 ${r.version ?? ""}，更新面板已在主窗口打开`);
      // 面板挂在主窗口上，不聚焦会被关于窗口挡住。
      void invoke("focus_main_window").catch(() => {});
      break;
    case "busy":
      showDialog("正在更新", "更新包正在下载，等这次装完再检查。");
      break;
    case "error":
      showDialog("检查更新失败", r.message ?? "网络或 GitHub 暂时连不上，选片不受影响。");
      break;
  }
});

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
