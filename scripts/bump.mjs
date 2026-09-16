#!/usr/bin/env node
// 改版本号。
//
// 版本号有三处，必须一致，否则打出来的包是 A 版本、关于窗口显示 B 版本：
//   package.json            → 前端
//   src-tauri/Cargo.toml    → 安装包
//   src-tauri/tauri.conf.json → 窗口标题、关于窗口、更新器
//
// 用法：node scripts/bump.mjs 0.1.1
// 之后：git commit -am "v0.1.1" && git tag v0.1.1 && git push --follow-tags
// （推上去之后 GitHub Actions 会自动出三平台的包并建 Release）

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const next = process.argv[2];

if (!next) {
  console.error("用法：node scripts/bump.mjs <版本号>   例如 node scripts/bump.mjs 0.1.1");
  process.exit(1);
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(next)) {
  console.error(`版本号看起来不对：${next}（期望形如 0.1.1 或 0.2.0-beta.1）`);
  process.exit(1);
}

function replace(path, pattern, build) {
  const file = join(root, path);
  const src = readFileSync(file, "utf8");
  const m = pattern.exec(src);
  if (!m) {
    console.error(`没在 ${path} 里找到版本号`);
    process.exit(1);
  }
  writeFileSync(file, src.replace(pattern, build(next)));
  console.log(`  ${path}: ${m[1]} → ${next}`);
}

console.log(`版本号 → ${next}`);
replace("package.json", /"version": "([^"]+)"/, (v) => `"version": "${v}"`);
replace("src-tauri/Cargo.toml", /^version = "([^"]+)"/m, (v) => `version = "${v}"`);
replace("src-tauri/tauri.conf.json", /"version": "([^"]+)"/, (v) => `"version": "${v}"`);

console.log(`
下一步：
  git add -A && git commit -m "v${next}"
  git tag v${next} && git push --follow-tags`);
