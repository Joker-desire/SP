# 参与开发

感谢你想改这个项目。下面是本机跑起来要做的、以及提交前必须过的检查。

## 跑起来

```bash
git clone https://github.com/Joker-desire/SP.git
cd SP
npm install
./run-dev.command        # 或 npm run tauri dev
```

需要 **Node ≥ 20.12** 和 **Rust stable**。`run-dev.command` 会自动挑合格的 node（很多人系统默认的是老版本）。

## 提交前跑一遍

```bash
npm run build                 # 类型检查 + 前端构建
cd src-tauri && cargo fmt     # 代码格式
cd src-tauri && cargo test    # 后端测试
```

CI（`.github/workflows/ci.yml`）会跑 `cargo fmt --check`、`cargo test`、`npm run build`，这三项不过 PR 会红。

## 代码约定

**Rust 侧**

- 所有磁盘操作都必须是 `src-tauri/src/lib.rs` 里显式 `#[tauri::command]` 暴露的命令。前端 WebView 没有文件系统权限——这不是限制，是「原片只读」这条原则的架构保障，别绕过。
- 会碰数据库的代码，锁只用来取元数据；解码、编码这类耗时活放在锁外，否则会把所有请求串行化掉。
- 新逻辑尽量写成可单测的纯函数（参考 `sizes_for`、`zoom.ts` 的写法），命令本身只做「取参数 → 调函数 → 打包返回值」。

**前端侧**

- 没有框架，原生 DOM。`src/main.ts` 按区块组织（缩略图队列 / 卡片 / 筛选 / 大图 / 缩放 …），新功能在对应区块里加，不要另起一个全局状态。
- 涉及几何换算的逻辑抽成纯函数放进独立模块（`src/zoom.ts` 是个例子），方便写断言。

**测试**

- 后端改动要有对应测试，尤其是「看起来不会错」的地方——这个项目里出过的问题（嵌套 JPEG 截断、WAL 残骸自愈、LIKE 通配符转义）都是靠测试堵住的。
- 需要真实素材才能验的部分，用 `#[ignore]` 加环境变量（`SP_SAMPLE` / `SP_SAMPLE_DIR`）而不是删掉。

## 提交与 PR

- 一个 PR 一件事，描述里说清楚「改了什么、为什么」。
- 界面如果有变化，贴一张截图——这个项目是看脸的（用户反馈过「太丑了」）。
- 不要在 PR 里塞格式化改动，它会淹没真正的修改（`cargo fmt` 单独跑）。

## 发版

版本由 maintainer 走：

```bash
npm run bump 0.1.1 && git commit -am "v0.1.1" && git tag v0.1.1 && git push --follow-tags
```

推 tag 之后 GitHub Actions 会自动构建 macOS（Apple Silicon / Intel）+ Windows 安装包并建 Release。版本号三处（package.json、Cargo.toml、tauri.conf.json）必须一致，所以用 `npm run bump`，不要手改。
