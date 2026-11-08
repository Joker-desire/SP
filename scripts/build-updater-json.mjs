// 由发版工作流的 updater-json job 调用：把三个平台上传的更新包与 .sig
// 汇总成 Tauri updater 要的静态清单 latest.json。
//
// 两条硬规矩：
// 1. 三个平台必须齐全。Tauri 会先校验整份 JSON 再看版本号，缺一个平台
//    会让所有客户端检查失败，所以宁可让 CI 红，也不能上传残缺清单。
// 2. signature 必须是 .sig 的内容，不能是路径或 URL。
import { existsSync, readFileSync, writeFileSync } from "node:fs";

const tag = process.env.SP_TAG;
if (!tag || !/^v\d+\.\d+\.\d+/.test(tag)) {
  console.error(`SP_TAG 不是版本 tag：${String(tag)}`);
  process.exit(1);
}

const version = tag.replace(/^v/, "");
const base = `https://github.com/Joker-desire/SP/releases/download/${tag}`;
const dir = "out-update";

// 文件名必须和 release.yml「收集更新产物」里改出来的名字逐字一致。
// Windows 选 .msi（配合 installMode: passive），不用 NSIS 的 setup.exe。
const targets = [
  ["darwin-aarch64", `SP_${version}_macos_apple-silicon.app.tar.gz`],
  ["darwin-x86_64", `SP_${version}_macos_intel.app.tar.gz`],
  ["windows-x86_64", `SP_${version}_windows_x64.msi`],
];

const platforms = {};
for (const [key, name] of targets) {
  const pkg = `${dir}/${name}`;
  const sig = `${pkg}.sig`;
  if (!existsSync(pkg) || !existsSync(sig)) {
    console.error(`缺少 ${name} 或它的 .sig；清单不完整，拒绝生成`);
    process.exit(1);
  }
  const signature = readFileSync(sig, "utf8").trim();
  if (!signature) {
    console.error(`${sig} 是空的，签名可能没生成（检查 CI 里的签名 Secrets）`);
    process.exit(1);
  }
  platforms[key] = { signature, url: `${base}/${name}` };
}

const manifest = {
  version,
  notes: `S·P ${version} —— 更新说明见 Release 页面`,
  pub_date: new Date().toISOString(),
  platforms,
};

writeFileSync("latest.json", `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`latest.json 已生成：${version}，${Object.keys(platforms).length} 个平台`);
for (const [key, value] of Object.entries(platforms)) {
  console.log(`  ${key} → ${value.url}`);
}
