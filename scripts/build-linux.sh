#!/usr/bin/env bash
# RustShell Linux 本地构建脚本（在 Linux 上执行）
#
# 为什么需要真机/CI：gpui-pre 的平台后端是**每个 OS 各一套**，gpui-pre-linux 要链
# X11 / Wayland / xkbcommon / fontconfig 与 GPU 栈，只能在 Linux 上编（且 feature 门
# 由 Cargo.toml 里的 `gpui-platform` 声明打开，见那里的注释）。
#
# 用法（从任意目录执行均可，脚本会自动定位仓库根）：
#   ./scripts/build-linux.sh
# 产物：dist/rustshell-<version>-linux-<x64|arm64>.tar.gz
#   用 tar.gz 而不是 zip：可执行位在 tar 里是原生的，解压后直接 ./rustshell 就能跑。
#
# 前置要求：
#   · rustup（stable toolchain）
#   · 系统开发包（Ubuntu / Debian）：
#       sudo apt-get install -y build-essential pkg-config \
#         libdbus-1-dev libx11-dev libxcb1-dev libfontconfig1-dev libfreetype-dev
#   · 运行时（运行产物的机器上）：libxkbcommon.so.0、libwayland-client.so.0、
#     libX11.so.6、fontconfig / freetype、libdbus-1，以及一份 Vulkan 或 OpenGL 驱动

set -euo pipefail
# 脚本位于 <repo>/scripts/，必须回到仓库根：VERSION 从根 Cargo.toml 读，
# cargo 与产物目录 dist/ 也都相对仓库根。
cd "$(dirname "$0")/.."

echo "==> 1/4 检查工具链与架构"
command -v cargo >/dev/null || { echo "未找到 cargo，请先安装 rustup"; exit 1; }

# 产物名里的架构直接取自本机 —— 构建是原生的（不交叉编译），所以名字必须跟着 uname 走。
# 名字写 x64 却编出 arm64 这种事在 CI 上是靠断言拦的，这里同理。
case "$(uname -m)" in
x86_64 | amd64) ARCH=x64 ;;
aarch64 | arm64) ARCH=arm64 ;;
*)
  echo "暂不支持的架构：$(uname -m)（目前只出 x64 / arm64 两种产物）" >&2
  exit 1
  ;;
esac
echo "    架构：$(uname -m) -> $ARCH"

echo "==> 2/4 检查系统依赖"
# 逐个 pkg-config 探测。这几条正是 Linux 目标依赖闭包里**真的会调 pkg-config** 的包
# （gpui-pre-linux → zed-scap → xcb → x11 crate；keyring → dbus-secret-service；
#  gpui-pre-wgpu → zed-font-kit → fontconfig / freetype）。它们各自的 build.rs 用的是
# `.unwrap()` 或直接 panic ⇒ 缺一个就编不过，所以在这里提前说清楚，
# 而不是让用户在 cargo build 的报错里大海捞针。
missing=""
for spec in "dbus-1:libdbus-1-dev" "x11:libx11-dev" "xcb:libxcb1-dev" "fontconfig:libfontconfig1-dev" "freetype2:libfreetype-dev"; do
  pc="${spec%%:*}"
  pkg="${spec##*:}"
  if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "$pc"; then
    echo "    ok   $pc"
  else
    echo "    缺失 $pc（apt 包：$pkg）"
    missing="$missing $pkg"
  fi
done
if [ -n "$missing" ]; then
  echo "缺少系统开发包，先执行（Ubuntu / Debian）：" >&2
  echo "  sudo apt-get install -y build-essential pkg-config$missing" >&2
  exit 1
fi

# 版本号提取**不要**用 `sed -E 's/version\s*=\s*"([^"]+)"/\1/'`：`\s` 是 GNU sed 扩展，
# 别的 sed（如 BSD sed）不认它 —— 会把 `\s` 当字面量 `s`，正则匹配不上、替换静默失败，
# VERSION 会变成整行 `version = "1.0.0"`（2026-10-16 CI 的 macOS 失败就是这个）。
# awk 按引号切分不涉及任何正则转义，GNU / BSD 行为一致。
VERSION=$(awk -F'"' '/^version/ { print $2; exit }' Cargo.toml)
if [ -z "$VERSION" ]; then
  echo "无法从 Cargo.toml 解析版本号（^version 行缺失或格式变了）" >&2
  exit 1
fi

echo "==> 3/4 构建 release（版本 $VERSION）"
cargo build --release -p rustshell

# 产物架构自检：名字里写了 x64 / arm64，就必须真是那个架构。
if command -v file >/dev/null 2>&1; then
  DESC=$(file -b target/release/rustshell)
  echo "    $DESC"
  case "$ARCH" in
  x64)
    echo "$DESC" | grep -q "x86-64" || { echo "产物不是 x86-64，与文件名不符" >&2; exit 1; }
    ;;
  arm64)
    echo "$DESC" | grep -q "aarch64" || { echo "产物不是 aarch64，与文件名不符" >&2; exit 1; }
    ;;
  esac
fi

echo "==> 4/4 打包"
STAGE="dist/rustshell-$VERSION-linux-$ARCH"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp target/release/rustshell "$STAGE/"
chmod 755 "$STAGE/rustshell"
rm -f "dist/rustshell-$VERSION-linux-$ARCH.tar.gz"
tar -czf "dist/rustshell-$VERSION-linux-$ARCH.tar.gz" -C dist "rustshell-$VERSION-linux-$ARCH"
ls -lh "dist/rustshell-$VERSION-linux-$ARCH.tar.gz"
echo "完成：dist/rustshell-$VERSION-linux-$ARCH.tar.gz（解压后在终端运行 ./rustshell）"
