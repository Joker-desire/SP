# 自动更新实施方案（v0.4.0 已实施）

> **状态**：已按本方案落地于 **v0.4.0（2026-11-08）**，下文保留为实施记录与运维手册。
> **适用**：首个带更新器的版本是 v0.4.0；v0.3.0 及更早版本无法自动升级，需手动装一次。
> **依据**：Tauri v2 Updater 官方文档与 `tauri-apps/tauri-action` README（2026-11-08 核对）。
> **关联**：`README.md` 下载安装 / 发版章节、`docs/DESIGN.md`、`CONTRIBUTING.md`。
>
> 实施时的两处偏差：① 改动落在 `feature/lens-filter-sidebar-collapse` 分支上，没有另开分支；② 客户端依赖与配置是手工精确编辑的，没用 `npm run tauri add`（避免它改写自定义 Builder 链）。密钥放在 `~/.tauri/sp-updater.key` + `.password`，两个值已写入仓库 Actions Secrets。

---

## 1. 目标与非目标

**目标**：装过一次新版本之后，应用能自己发现 GitHub Release 上的新版本，验签、下载、安装、重启，全程用户可拒绝；不需要再手动去 Release 页下 dmg。

**非目标**：
- 不做后台常驻轮询、不做「监控 GitHub 打包产物」的服务。
- 不自建更新服务器、不引入第三方分发平台（CrabNebula 等）。
- 不改变现有产物命名（`SP_0.3.0_macos_apple-silicon.dmg` 这套）、不改变「谁先到谁建 Release」的并行上传逻辑。
- 不改动用户数据目录 `~/Library/Application Support/SP/`——更新只替换 app bundle，索引库和选片标记不动。

---

## 2. 为什么不自己轮询 GitHub Releases API

| 自己轮询 | 官方 updater + 静态清单 |
|---|---|
| 未认证 60 次/小时限流；token 不可能塞进客户端 | 一次 GET 一个静态 JSON 文件，无限流概念 |
| 要自己判 semver、按 os/arch 挑包、解析 tag | 插件按 `{{target}}` / `{{arch}}` 匹配 `platforms` 键 |
| 要自己下载、验签、替换 app、处理 Windows 安装器退出 | `downloadAndInstall()` 已封装，**签名验证不可关闭** |
| 要自己处理回滚与并发 | 版本比较、超时、代理、请求头都有现成 API |

「监控」这件事真正该发生在 **CI 侧**：发版时把签名和清单算好、传上去。客户端只在启动时或用户点「检查更新」时读一次。

---

## 3. 现状差距（逐文件）

| 文件 | 实施前 | v0.4.0 现状 |
|---|---|---|
| `src-tauri/tauri.conf.json` | 无 `bundle.createUpdaterArtifacts`，无 `plugins` 段 | 开启更新产物 + 写 `pubkey` / `endpoints` |
| `src-tauri/Cargo.toml` | 无 `tauri-plugin-updater`、无 `tauri-plugin-process` | 新增两个依赖 |
| `package.json` | 无 `@tauri-apps/plugin-updater`、无 `@tauri-apps/plugin-process` | 新增两个依赖（`relaunch()` 来自后者） |
| `src-tauri/src/lib.rs` | `Builder::default()` 链在 2868 行附近 | 注册两个插件 |
| `src-tauri/capabilities/default.json` | `windows: ["main"]`，权限只有 core/opener/dialog | 加 `updater:default`、`process:default`；**about 窗口 label 是 `"about"`，不在这个数组里** |
| `.github/workflows/release.yml` | `tauri-action@v0` 不传 `tagName`（只构建），自己 `gh release create` 上传改名的 dmg/msi/exe | 构建时注入签名密钥；新增收集更新产物的步骤；新增汇总 job 生成并上传 `latest.json` |
| 密钥 | 无 | 需要一对 minisign 密钥，私钥托管到 GitHub Secrets 并离线备份 |

**工作流的关键冲突**：tauri-action 的 `uploadUpdaterJson`（默认 `true`）要生效，得由 action 自己管理 Release；而当前流程刻意不给 `tagName`、自行建 Release 并重命名产物。另外当前锁的是 `@v0`，官方示例已是 `@v1`，这些 input 在 v0 上不一定齐备。因此**推荐方案 B：保留现有流程，自己用汇总 job 生成 `latest.json`**，不把 Release 控制权交回 action。

---

## 4. 总体链路

```
打 tag  →  三个平台并行 build（带签名密钥）
              ├─ 原流程：上传改名后的 dmg / msi / exe
              └─ 新增：上传 .app.tar.gz / .msi 及各自的 .sig 为 workflow artifacts
        →  汇总 job（needs: build）
              生成 latest.json（三平台签名齐全才允许上传）
              gh release upload --clobber
        →  GitHub Release 资产：latest.json + 更新包
        →  客户端 check() → 版本比较 → 验签 → downloadAndInstall() → relaunch()
```

`endpoints` 固定为 `https://github.com/Joker-desire/SP/releases/latest/download/latest.json`。

---

## 5. 实施步骤

### 步骤 0：分支

原计划从 `main` 另开 `feature/auto-update`；实际评审后与侧栏多选筛选一起落在 `feature/lens-filter-sidebar-collapse` 分支上，统一发 v0.4.0。以后单独改更新逻辑时再从 `main` 另开分支。

### 步骤 1：生成签名密钥（人工，必须做在最前）

```bash
npm run tauri signer generate -- -w ~/.tauri/sp-updater.key
```

命令会提示设置密码，生成 `~/.tauri/sp-updater.key`（私钥）与 `.key.pub`（公钥）。

1. **公钥**：把 `.key.pub` 的全文（一整行 base64）填进 `tauri.conf.json` 的 `plugins.updater.pubkey`。公钥可公开。
2. **私钥**：仓库 → Settings → Secrets and variables → Actions，建两个 repository secret：
   - `TAURI_SIGNING_PRIVATE_KEY`：私钥全文（多行原样粘贴）
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：上一步的密码（留空则可不建）
3. **离线备份**：私钥与密码存密码管理器或加密介质。`.gitignore` 已忽略 `*.key` / `src-tauri/keys/`，但**私钥绝不能进仓库**。

> ⚠️ **私钥丢了就再也推不了更新**。公钥编译进了已安装的 app，无法事后更换；届时只能让用户全部手动重装。这是整套方案里唯一不可逆的一步。

### 步骤 2：客户端接入

实际做法：**手工精确编辑** `Cargo.toml`、`package.json`、`capabilities/default.json`、`lib.rs`，然后 `npm install` / `cargo check` 更新 lock。没有用 `npm run tauri add updater`——项目的 `lib.rs` 是自定义 Builder 链，自动插入容易改坏；手工改也更好审。最终版本：`tauri-plugin-updater 2`、`tauri-plugin-process 2`、前端 `@tauri-apps/plugin-updater ^2` 与 `@tauri-apps/plugin-process ^2`。

**`src-tauri/src/lib.rs`**，在 2868 行的 Builder 链里：

```rust
let builder = tauri::Builder::default()
    .plugin(tauri_plugin_opener::init())
    .plugin(tauri_plugin_dialog::init())
    .plugin(tauri_plugin_updater::Builder::new().build())
    .plugin(tauri_plugin_process::init())
    // ...原有 window_state / single_instance 保持不动
```

**`src-tauri/tauri.conf.json`**（与 `bundle` 平级新增 `plugins`）：

```json
{
  "bundle": {
    "active": true,
    "targets": "all",
    "createUpdaterArtifacts": true,
    "resources": {},
    "icon": ["icons/32x32.png", "icons/128x128.png", "icons/128x128@2x.png", "icons/icon.icns", "icons/icon.ico"]
  },
  "plugins": {
    "updater": {
      "pubkey": "<步骤 1 的公钥全文>",
      "endpoints": [
        "https://github.com/Joker-desire/SP/releases/latest/download/latest.json"
      ],
      "windows": { "installMode": "passive" }
    }
  }
}
```

开启 `createUpdaterArtifacts` 后，macOS 多产出 `S·P.app.tar.gz` + `.sig`，Windows 多产出 `.msi.sig` / `.exe.sig`。现有 `release.yml` 的 `find` 只匹配 `*.dmg|*.msi|*.exe|*.deb|*.AppImage`，不会误传这些新文件。

**`src-tauri/capabilities/default.json`**：

```json
"permissions": [
  "core:default",
  "opener:default",
  "dialog:default",
  "updater:default",
  "process:default"
]
```

若 `process:default` 不含 `allow-relaunch`（以本机生成的 schema 为准），显式补一条 `process:allow-relaunch`。

> 关于窗口的 label 是 `"about"`，不在 `default.json` 的 `windows: ["main"]` 里。现在「检查更新」按钮放在关于页，因此另有一份 `capabilities/about.json` 只给它 `core:default`（用于事件 emit / listen）。updater / dialog 插件权限仍只属于主窗口：about 页发 `sp://update-check-request`，主窗口执行 `check()`，再用 `sp://update-check-result` 把 `latest` / `available` / `error` / `busy` 送回，about 页负责弹窗。跨窗口传的是普通对象，不是 `Update` 实例，所以主窗口始终持有下载安装所需的真实对象。

**`src/main.ts`**（主窗口，示意骨架）：

```ts
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

async function checkForUpdate(silent: boolean) {
  try {
    const update = await check({ timeout: 15000 });
    if (!update) {
      // 手动检查：状态栏 + 关于窗口的「已是最新版本」弹窗
      if (!silent) {
        setHint("已是最新版本");
        emit(UPDATE_CHECK_RESULT, { kind: "latest" });
      }
      return;
    }
    // 弹「发现 vX：立即更新 / 稍后」，逐条展示 update.body 里的更新项
    if (!(await confirmUpdate(update.version, update.body))) return;
    await update.downloadAndInstall((event) => {
      if (event.event === "Progress") setProgress(event.data.chunkLength);
    });
    await relaunch();
  } catch (e) {
    console.warn("检查更新失败", e);
    if (!silent) toast("检查更新失败，可到 Release 页手动下载");
  }
}

// 启动 8 秒后静默查一次，不阻塞首屏；手动入口在「关于 S·P」窗口里，
// 它只发事件，真正的 check() 仍在主窗口执行
setTimeout(() => void checkForUpdate(true), 8000);
```

约定：**失败静默降级、不阻塞启动、绝不强制更新**（用户可能正在连续打标签，突然重启很烦），并且永远保留手动下载 Release 的入口。

### 步骤 3：发版流程改造（方案 B）

**3.1 打包步骤注入密钥**——在 `release.yml` 的「打包」step 加 env。签名是 Tauri CLI 打包时读环境变量，action 只是调用 CLI，因此 step 级 env 即可透传：

```yaml
      - name: 打包
        uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
```

落地前先本地验证一次（能产出 `.sig` 才算透传成功）：

```bash
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/sp-updater.key)" \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD="..." \
  npm run tauri build -- --bundles app
```

若 `@v0` 不透传，改为升 `@v1`，或把「打包」换成自建的 `npm run tauri build` 步骤（现有自定义上传逻辑不依赖 action 的上传能力）。

**3.2 收集更新产物**——在「重命名产物」之后新增一步，把更新包和签名复制到 `out-update/` 并改成 ASCII 名（原始文件名含 `S·P` 的间隔号，上传时会被换成 `.`）。**每个 step 都是独立 shell，上一步 `export` 的变量不会继承过来，这里必须自己重算一遍**：

```yaml
      - name: 收集更新产物
        if: startsWith(github.ref, 'refs/tags/')
        shell: bash
        run: |
          v="${GITHUB_REF_NAME#v}"
          os="${{ matrix.os }}"; os="${os%-latest}"
          arch="${{ matrix.arch }}"
          export v os arch   # find -exec 起的是子进程，不 export 就取不到
          mkdir -p out-update
          find src-tauri/target -name '*.app.tar.gz' \
            -exec sh -c 'cp "$1" "out-update/SP_${v}_macos_${arch}.app.tar.gz"' _ {} \;
          find src-tauri/target -name '*.app.tar.gz.sig' \
            -exec sh -c 'cp "$1" "out-update/SP_${v}_macos_${arch}.app.tar.gz.sig"' _ {} \;
          find src-tauri/target -name '*.msi' \
            -exec sh -c 'cp "$1" "out-update/SP_${v}_windows_x64.msi"' _ {} \;
          find src-tauri/target -name '*.msi.sig' \
            -exec sh -c 'cp "$1" "out-update/SP_${v}_windows_x64.msi.sig"' _ {} \;
          ls -lh out-update
```

签名校验的是**文件内容**，不是文件名，重命名不影响验签。

**3.3 每个 build job 上传更新产物为 workflow artifact**：

```yaml
      - name: 上传更新产物
        if: startsWith(github.ref, 'refs/tags/')
        uses: actions/upload-artifact@v4
        with:
          name: update-${{ matrix.arch }}       # apple-silicon / intel / x64，互不覆盖
          path: out-update/*
          if-no-files-found: error
```

**3.4 新增汇总 job**（与 `build` 平级）：

```yaml
  updater-json:
    needs: build
    if: startsWith(github.ref, 'refs/tags/')
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
      - uses: actions/download-artifact@v4
        with:
          pattern: update-*
          path: out-update
          merge-multiple: true
      - name: 生成 latest.json
        env:
          SP_TAG: ${{ github.ref_name }}
        run: node scripts/build-updater-json.mjs
      - name: 上传清单与更新包
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          SP_TAG: ${{ github.ref_name }}
        run: gh release upload "$SP_TAG" out-update/* latest.json --clobber
```

`needs: build` 保证三个平台都跑完、Release 也已由原流程创建，这里只需 `--clobber` 追加/覆盖。`workflow_dispatch` 手动触发不建 Release，用 `if` 跳过整个 job。

**3.5 新增 `scripts/build-updater-json.mjs`**，核心逻辑：

```js
import { readFileSync, writeFileSync, existsSync } from "node:fs";

const tag = process.env.SP_TAG;              // v0.4.0
const version = tag.replace(/^v/, "");
const base = `https://github.com/Joker-desire/SP/releases/download/${tag}`;

const targets = [
  ["darwin-aarch64",  `SP_${version}_macos_apple-silicon.app.tar.gz`],
  ["darwin-x86_64",   `SP_${version}_macos_intel.app.tar.gz`],
  ["windows-x86_64",  `SP_${version}_windows_x64.msi`],
];

const platforms = {};
for (const [key, name] of targets) {
  const sigPath = `out-update/${name}.sig`;
  const pkgPath = `out-update/${name}`;
  // 三个平台必须齐全：Tauri 会先校验整个 JSON，缺一项会让所有客户端检查失败
  if (!existsSync(sigPath) || !existsSync(pkgPath)) {
    console.error(`缺少更新产物：${name} 或它的 .sig`);
    process.exit(1);
  }
  platforms[key] = {
    signature: readFileSync(sigPath, "utf8").trim(),
    url: `${base}/${name}`,
  };
}

writeFileSync("latest.json", JSON.stringify({
  version,
  notes: readFileSync(`release-notes/${tag}.md`, "utf8").trim(),
  pub_date: new Date().toISOString(),   // RFC 3339
  platforms,
}, null, 2));
```

要点：
- `platforms` 键是 `OS-ARCH` 格式；`signature` 必须是 `.sig` 的**内容**，不能是路径或 URL。
- **任一平台缺签名就让 CI 失败**，绝不上传一份残缺清单。
- `notes` 来自 `release-notes/<tag>.md`（每行一条 `- xxx`），同一份内容也用作 Release 页说明。缺文件或没有 `- ` 条目直接让 CI 失败，客户端会把这些条目渲染成列表；**不允许再发「更新说明见 Release 页面」这类占位文案**。
- Windows 选 `.msi` 而非 NSIS `.exe`，配合 `installMode: "passive"`；若改选 exe，对应改 `updaterJsonPreferNsis` 的等价决策。
- 带连字符的 tag（如 `v0.4.1-beta.1`）会被原流程标成 `--prerelease`，`/releases/latest/` **不会**指向它，灰度别用这种 tag。

### 步骤 4：首次发版与灰度

1. `node scripts/bump.mjs 0.4.0`，提交并推 `v0.4.0` tag。
2. 等工作流全绿，确认 Release 里除四个安装包外还有 `latest.json`、两个 `.app.tar.gz`、一个 `.msi` 及对应 `.sig`。
3. **手动安装一次 v0.4.0** —— 已装的 v0.3.0 里没有 updater 插件也没有公钥，永远不会自动升级，这个断层只能靠手动跨过去。
4. 再发一个 `v0.4.1`（改一行文案即可），验证 v0.4.0 → v0.4.1 的完整自动更新链路。
5. 灰度期内保留 Release 页手动下载入口。

**首次灰度执行记录（2026-11-08）**：`v0.4.1` 已作为正式 tag 发出（不带 `-` 后缀，否则 `/releases/latest/` 会跳过 prerelease，客户端永远读不到新清单；这一版只升版本号，无功能改动）。发布 run `42489269914` 四个 job 全绿；Release 10 个资产齐全；`/releases/latest/download/latest.json` 返回 200，`version: 0.4.1`，三个平台的签名长度 400 / 400 / 412，`darwin-aarch64` 指向 7.51 MB 的 `SP_0.4.1_macos_apple-silicon.app.tar.gz`。本机已确认 `/Applications/S·P.app` 为 0.4.0、内置公钥与 CI 签名私钥配套。

**客户端验证结果（2026-11-09，用户实测通过）**：v0.4.0 → v0.4.1 的完整链路走通 —— 检查发现新版 → 下载 → 验签 → 安装 → 自动重启 → 关于页显示 0.4.1，选片数据与标记未受影响。至此首次灰度用例闭环，自动更新通道从「服务端就绪」升级为「端到端已验证」。

**v0.4.2 发布记录（2026-11-09）**：首个带 `release-notes/<tag>.md` 的正式版。发布 run `42560851891` 四个 job 全绿；Release 10 个资产齐全；`/releases/latest/download/latest.json` 返回 200、`version: 0.4.2`、`notes` 含 6 条 `- xxx`，三平台签名长度 400 / 400 / 412，`darwin-aarch64` 指向 7.52 MB 的 `SP_0.4.2_macos_apple-silicon.app.tar.gz`；Release 页说明与清单同源（`--notes-file`），同样 6 条。

> **跨版本显示差异（别误判成功能没生效）**：从 0.4.1 收到 0.4.2 时，更新面板仍由 0.4.1 的旧前端渲染，`notes` 会以带 `- ` 前缀的纯文本显示，而不是项目列表；逐条渲染成 `<ul>` 是 0.4.2 自带的代码，要等更新完成、下一次发版（0.4.3）才会看到列表效果。同理，「关于 S·P」里的「检查更新」按钮也是 0.4.2 装好后才有。

> 触发检查前先**重启应用**（0.4.1 顶栏的下载箭头也可手动检查）：启动时那次静默检查只在 8 秒后跑一次，代码里没有轮询，已经跑过就不会自己再查。刚发布时 GitHub 的 `/releases/latest/` 可能有短暂 CDN 缓存，若提示「已是最新版本」，等一两分钟重试即可。

### 步骤 5：验证清单

| 项 | 期望 |
|---|---|
| `npm run build`、`cargo check`、`cargo fmt --check` | 全过 |
| 本地带密钥打包 | 产出 `.app.tar.gz` 与 `.sig` |
| CI 三个 build job | 各自产出 sig，`updater-json` job 生成并上传 `latest.json` |
| `curl -fsSL .../releases/latest/download/latest.json \| jq .` | 三个 platforms 齐全，`version` 为 semver，`notes` 含该版 `- xxx` 条目 |
| `release-notes/<tag>.md` | 更新面板逐条列出更新项；文件缺失或没有条目时 CI 直接失败 |
| 装 v0.4.0 → 检查 → 下载 → 安装 → 重启 | 关于页版本号变成 0.4.1（**2026-11-09 实测通过**） |
| v0.4.1 → 检查到 v0.4.2 | 面板列出这一版的 6 条更新项（旧客户端显示为纯文本），关于页版本号变 0.4.2 |
| 0.4.2 之后再发一版 | 更新项渲染成逐条列表；关于页「检查更新」可用 |
| 关于页点「检查更新」且没有新版本 | 弹窗提示「已是最新版本」并带当前版本号，按钮恢复可点 |
| Windows / Linux 点顶栏 ⓘ | 能打开同一个关于窗口，检查更新按钮可用 |
| 断网 / GitHub 超时时启动 | 不卡首屏、无报错弹窗、可继续正常选片 |
| 篡改 `.sig` 或包内容 | 验签失败并拒绝安装 |
| macOS 更新后 | 记录 Gatekeeper 是否重新拦截（当前 app 未签名/未公证） |
| 用户数据 | `library.db` 与选片标记、缩略图缓存均不受影响 |

本机实测（2026-11-08）：`github.com` 直连 8 秒超时（仅收到 344KB），`objects.githubusercontent.com` 可达（0.39s / 404）。因此 `check({ timeout: 15000 })` + 失败静默降级 + 手动下载入口是**必需项**，不是锦上添花。

---

## 6. 风险、约束与回滚

| 风险 | 影响 | 处理 |
|---|---|---|
| 私钥丢失 | 不可逆：所有已装用户再也收不到更新 | 离线备份；不要提交进仓库 |
| v0.3.0 断层 | 老版本永远停在 0.3.0 | README / Release Notes 明确告知需手动装 v0.4.0 |
| `latest.json` 残缺 | 所有平台客户端检查失败 | 生成脚本强制三平台齐全才允许上传 |
| GitHub 网络不稳 | 检查失败或下载中断 | 超时 + 静默降级 + 手动下载；不要自动重试轰炸 |
| 未签名 / 未公证的 macOS app | 更新后 Gatekeeper 可能再拦一次 | 现状本来就要手动绕过；后续可接入 Apple 签名，不在本方案范围 |
| 更新打断选片 | 用户正在连续打标签时丢上下文 | 检查与新版本提示都不打断，由用户选「立即 / 稍后」；但**点了「立即更新」后，下载装好会立刻 `relaunch()`**，所以别在批量打标签或扫描中途点。已写入 SQLite 的标记不丢，丢的是当前筛选、滚动位置和进行中的扫描。后续可改成「重启以完成更新」由用户择时 |

**回滚**：出问题时删掉 Release 上的 `latest.json`，或把它重传成旧版本号即可；已安装的 app 不会被破坏，只是检查不到新版本。更新失败不会损坏数据目录。

---

## 7. 最终采用的选择

1. **检查时机**：启动 8 秒后静默查一次 + 关于页「检查更新」按钮手动查；不做定时轮询。查到过就直接把面板叫回来，不重复打网络请求。
2. **更新说明**：来自 `release-notes/<tag>.md`，每行一条 `- xxx`；发版时写进 `latest.json` 的 `notes`，同时作为 Release 页说明，客户端面板用 `textContent` 逐条渲染成列表（不解析 HTML，内容来自 GitHub 不可信）。缺文件 / 无条目时建 Release 与生成清单两步都会让 CI 红。不提供「跳过此版本」。
3. **手动检查的反馈**：已是最新版本 / 失败 / 下载中 / 发现新版，四种结果都经事件回传到关于窗口 —— 无更新时**弹窗写明当前版本号**，失败给原因与手动下载入口；主窗口状态栏仍同步一句 `setHint`。about 侧有 20 秒超时兜底，主窗口没回应也不会把按钮一直锁住。
4. **Windows**：用 `.msi` + `installMode: "passive"`，不用 NSIS `.exe`。
5. **关于页**：手动检查按钮就在这里；顶栏的 ⓘ 是「关于」入口（也是 Windows / Linux 唯一入口，小红点挂它上面）。检查逻辑仍全在主窗口，about 窗口只发事件、显示结果，权限由独立的 `capabilities/about.json` 给 `core:default`，`default.json` 的 `windows` 仍是 `["main"]`。查到新版时调 `focus_main_window` 把主窗口提到前面，否则更新面板会被关于窗口挡住。
6. **镜像 endpoint**：暂不加，只用 `/releases/latest/download/latest.json`；靠 `check({ timeout: 15000 })`、失败静默降级与常驻的手动下载入口兜底。
7. **预发布版**：带连字符的 tag 会被标 `--prerelease`，`/releases/latest/` 不指向它，所以自动更新只认正式版 tag；测试包走 `workflow_dispatch` 的 Artifacts。

---

## 8. 明确不做

- 不做差分更新（Tauri v2 原生 updater 没有内置差分）。
- 不做静默强制更新、不做使用统计上报。
- 不引入更新服务器、不引入第三方 CDN 依赖。
- 不改现有产物命名与下载计数习惯。
