# <img src="branding/icon-final.png" width="52" valign="middle" alt="S·P logo" /> S·P 选片

> **S·P —— Select Photo。Shoot thousands. Keep the best.**
> 千张过手，好片留下。

![CI](https://github.com/Joker-desire/SP/actions/workflows/ci.yml/badge.svg)
![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)

本地选片工具：一次几千张照片，几分钟挑完。**原片全程只读，数据只留本机**——不联网、不上传、不注册账号。

| | |
|---|---|
| 平台 | macOS（Apple Silicon / Intel）、Windows |
| 技术栈 | Tauri 2 + Rust + SQLite，前端 Vite + TypeScript（无框架） |
| 许可 | MIT |

## 能做什么

- **NEF / JPG 自动配对** —— 同目录同名视为一次快门，直接抠 RAW 内嵌预览看图，不用等完整解码
- **分组 + 多选筛选** —— 按日期 / 镜头 / 机身 / ISO / 文件夹等分组；同维并集、跨维交集，已选条件置顶、一键清除
- **键盘选片** —— `P` 保留、`X` 淘汰、`0-5` 星级、`6-9` / `-` 色标，`⌘Z` 撤销；连拍组内可保留多张
- **批量与导出** —— 对筛选结果一次性标记，导出只**复制**原片并生成 CSV 清单
- **几千张不卡** —— 缩略图分级缓存、视口懒加载、滚远自动释放位图
- **一步标记 + 自定义列数** —— 鼠标停在哪张就按数字给哪张打星，卡片上也能直接点星；列数 1–8 可调（2 列看图更大）
- **简洁 / 专业一键切换** —— 默认只留导入、看图、标记、导出；高级功能收起但快捷键通用
- **应用内自动更新** —— 启动静默检查，发现新版逐条列出更新项，确认后才下载安装

> 完整使用说明、快捷键、彩蛋与实现细节见 [`docs/GUIDE.md`](docs/GUIDE.md)。

---

## 下载安装

到 [Releases](https://github.com/Joker-desire/SP/releases) 下载对应平台的安装包，双击安装即可，**不需要 Node、Rust 或任何运行环境**：

| 平台 | 文件 |
|---|---|
| macOS（Apple Silicon，M 系列） | `SP_<版本>_macos_apple-silicon.dmg` |
| macOS（Intel） | `SP_<版本>_macos_intel.dmg` |
| Windows | `SP_<版本>_windows_x64.msi`（或 `.exe`，二选一） |

数据（图库索引、选片标记、缩略图缓存）在 macOS `~/Library/Application Support/SP/`、Windows `%APPDATA%\SP\`。**删应用不会删照片**——原片只读，重装后标记还在。

**macOS 首次打开**：应用没有 Apple 开发者签名，浏览器下载的文件带隔离属性，双击会提示「应用已损坏」，右键 → 打开也不管用。拖进「应用程序」后执行一行即可，之后双击正常启动：

```bash
xattr -cr "/Applications/S·P.app"
```

Windows 遇到 SmartScreen 提示点「仍要运行」。

---

## 自动更新

- 启动约 8 秒静默检查一次；手动查：顶栏 ⓘ「关于 S·P」→「检查更新」，**已是最新版本会弹窗明确告知**
- 发现新版时面板**逐条列出这一版的更新项**（来自 `release-notes/<tag>.md`），由你选「立即更新」或「稍后」；下载安装完成后重启。检查失败不影响选片，下载 / 安装失败会在面板里写明原因（v0.4.3 起）
- 更新包带 minisign 签名，验签不过一律拒装。若 GitHub Release 大文件被网关拦截（`504`），直接从 Releases 装对应 dmg / msi
- **v0.3.0 及更早版本没有内置更新器**，请手动装一次 v0.4.0，之后就能在应用内更新；数据目录与标记都不受影响

机制、签名密钥与故障排查见 [`docs/AUTO_UPDATE.md`](docs/AUTO_UPDATE.md)。

---

## 三步上手

1. **选文件夹** —— 有子目录时先勾选扫描范围，选完即自动建索引、边扫边出图
2. **看图标记** —— 顶栏挑分组依据，点卡片进大图；`P` / `X` / `0-5` 打标记，`⌘Z` 撤销
3. **导出** —— 筛选或勾选要的照片，导出按模板复制到目标文件夹，原片不动

| 常用键位 | 作用 |
|---|---|
| `P` / `X` / `U` | 保留 / 淘汰 / 清除 |
| `0`–`5`、`6`–`9` / `-` | 星级、色标 |
| `⌘Z` / `⇧⌘Z` | 撤销 / 重做 |
| `←` `→` / `Esc` | 大图翻页 / 关闭 |
| `I` / `F` / `\` | 详情 / 全屏 / 收起侧栏 |

---

## 更新记录

| 版本 | 主要变化 |
|---|---|
| [v0.4.3](https://github.com/Joker-desire/SP/releases/tag/v0.4.3) · 2026-11-09 | 更新失败时在面板显示分类中文原因；Release 大文件 504 时提供手动安装兜底 |
| [v0.4.2](https://github.com/Joker-desire/SP/releases/tag/v0.4.2) | 连拍组可保留多张；更新面板逐条列更新项；手动检查更新移到关于页；修复浅色主题按钮 hover 白底白字 |
| [v0.4.1](https://github.com/Joker-desire/SP/releases/tag/v0.4.1) | 自动更新链路端到端验证版，无功能改动 |
| [v0.4.0](https://github.com/Joker-desire/SP/releases/tag/v0.4.0) | 应用内自动更新；镜头筛选、侧栏折叠、全分面多选与已选条件区 |
| [v0.3.0](https://github.com/Joker-desire/SP/releases/tag/v0.3.0) | 移除闭眼检测、只保留分组视图、UI 重做；修复连续打标签卡顿与「预览提取失败」 |
| [v0.2.0](https://github.com/Joker-desire/SP/releases/tag/v0.2.0) | 对焦框、重做栈、窗口记忆与单实例、全屏看图、选片进度、批量标记 |
| [v0.1.0](https://github.com/Joker-desire/SP/releases/tag/v0.1.0) | 首个版本：分组、完整 EXIF、撤销、画面质量筛选与导出 |

完整说明见各版本的 `release-notes/<tag>.md` 或 Releases 页面。

---

## 开发

双击 `run-dev.command`（或 `./run-dev.command`）即可从源码运行，需要 **Node ≥ 20.12** 与 **Rust stable**；首次编译约 3–5 分钟，之后改前端热重载。

推一个 `v*` 的**附注 tag**，GitHub Actions 会自动打 macOS 双架构 + Windows 安装包、生成 `latest.json` 并建 Release。每次发版必须写 `release-notes/<tag>.md`（每行一条 `- xxx`），否则 CI 直接失败。

开发约定、测试命令、完整发版清单见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/GUIDE.md`](docs/GUIDE.md) | 使用说明、快捷键、彩蛋、常见问题与实现细节 |
| [`docs/DESIGN.md`](docs/DESIGN.md) | 设计原则、架构、数据模型、路线图与风险清单 |
| [`docs/AUTO_UPDATE.md`](docs/AUTO_UPDATE.md) | 自动更新方案、签名密钥、发版工作流与故障排查 |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | 开发环境、代码约定、测试与发版 |

---

## 参与开发

提 bug / 建议：[Issues](https://github.com/Joker-desire/SP/issues)。改动时别破坏两条底线：

1. **原片只读** —— 不删除、不移动、不重命名、不改写原片；导出只复制
2. **选片标记按 `pair_key` 存** —— 换盘、挪目录、重建索引都不能丢，这是 `decisions` 表存在的全部理由

## 许可

[MIT](LICENSE)。可以随便用、改、分发，包括商用；唯一义务是保留版权声明。图标 [`branding/icon-final.png`](branding/icon-final.png) 随代码一同按 MIT 授权。
