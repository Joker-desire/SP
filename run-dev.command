#!/bin/bash
# S·P · 开发模式启动
#
# 双击本文件，或在终端里执行 ./run-dev.command
#
# 首次运行会编译 Rust（约 3-5 分钟），之后启动是秒级。
# 改动前端（index.html / src/）会热重载，不用重启。

cd "$(dirname "$0")" || exit 1

export PATH="$HOME/.cargo/bin:$PATH"

# 本机 npm 全局缓存目录里有 root 属主的文件（会导致 EPERM），
# 所以这里指向一个隔离缓存，避免动到系统目录。
export npm_config_cache="${npm_config_cache:-/tmp/sp-npm-cache}"

# ---------------------------------------------------------------
# 找 Node：必须 >= 20.12（Vite 8 需要 node:util.styleText）
# 本机 /usr/local/bin/node 是 16.x，直接用会报 SyntaxError，
# 所以这里显式挑一个合格的，不信任 PATH 里的默认 node。
# ---------------------------------------------------------------
BEST_NODE=""
BEST_MAJOR=0
BEST_MINOR=0
BEST_VER=""

consider() {
  local bin="$1"
  [ -x "$bin" ] || return 0
  # 直接用 Vite 真正依赖的那个能力做检测，比解析版本号可靠
  "$bin" -e "process.exit(require('node:util').styleText ? 0 : 1)" >/dev/null 2>&1 || return 0

  local ver major minor
  ver="$("$bin" -e 'process.stdout.write(process.versions.node)' 2>/dev/null)"
  [ -n "$ver" ] || return 0
  major="${ver%%.*}"
  minor="${ver#*.}"; minor="${minor%%.*}"

  if [ "$major" -gt "$BEST_MAJOR" ] ||
     { [ "$major" -eq "$BEST_MAJOR" ] && [ "$minor" -gt "$BEST_MINOR" ]; }; then
    BEST_NODE="$bin"
    BEST_MAJOR="$major"
    BEST_MINOR="$minor"
    BEST_VER="$ver"
  fi
}

consider "$HOME/.local/node/bin/node"
consider /opt/homebrew/bin/node
consider /opt/homebrew/opt/node/bin/node
consider /usr/local/opt/node/bin/node
for p in "$HOME"/.nvm/versions/node/*/bin/node \
         "$HOME"/.workbuddy/binaries/node/versions/*/bin/node \
         /usr/local/bin/node \
         /usr/bin/node; do
  consider "$p"
done

if [ -z "$BEST_NODE" ]; then
  echo "✗ 找不到可用的 Node.js（需要 20.12 或更高）。"
  echo "  本机 /usr/local/bin/node 是 $(/usr/local/bin/node --version 2>/dev/null)，太旧。"
  echo
  echo "  装一个 Node 22 就行："
  echo "    brew install node"
  echo "  或从 https://nodejs.org 下载 LTS 安装包。"
  exit 1
fi

export PATH="$(dirname "$BEST_NODE"):$PATH"

if ! command -v cargo >/dev/null 2>&1; then
  echo "✗ 找不到 cargo。请先确认 Rust 已安装："
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  exit 1
fi

echo "S·P · 开发模式"
echo "  Node v$BEST_VER  →  $BEST_NODE"
echo "  首次编译需要几分钟，之后启动是秒级。"
echo

npm run tauri dev
