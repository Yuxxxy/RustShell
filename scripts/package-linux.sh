#!/usr/bin/env bash
# RustShell Linux 安装包（.deb）
#
# 用法（从任意目录执行均可，脚本会自动定位仓库根）：
#   ./scripts/package-linux.sh                    # 编译并打包（本地用法；架构按 uname -m）
#   ./scripts/package-linux.sh <二进制路径>        # 只打包已编好的二进制（CI 用法）
#   ./scripts/package-linux.sh <二进制> arm64      # 显式指定架构（跨架构打包时用）
#
# 产物：dist/RustShell-linux-<x64|arm64>-<version>.deb
#   命名规则全仓唯一：`RustShell-<平台>-<版本>.<扩展名>`。
#
# 为什么手写 dpkg-deb 而不用 cargo-packager：
#   · packager 的 deb 后端会按 `[package.metadata.packager]` 走它的通用配置
#     （bundle 目录结构、before-packaging-command 会再触发一次 cargo build），
#     在 Linux 上等于把 CI 的编译重做一遍；而我们要的只是「一个二进制 + 一份
#     .desktop + 一个图标」这么点东西。
#   · 手写之后产物完全由仓库内文件决定（app/assets/ 三件套 + LICENSE），
#     没有第二处配置会漂移；也不需要额外装 20 多分钟的 cargo-packager。
#
# 前置：dpkg-deb（Debian / Ubuntu 自带，属 dpkg 包；CI 的 ubuntu runner 一定有）
#       编译路径还需要 rustup 与下列开发包（缺了脚本会逐条报出 apt 包名）：
#         build-essential pkg-config cmake libdbus-1-dev libx11-dev libxcb1-dev
#         libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev libfreetype-dev

set -euo pipefail
# 脚本位于 <repo>/scripts/，必须回到仓库根：VERSION 从根 Cargo.toml 读，
# 产物目录 dist/ 与 app/assets/ 也都相对仓库根。
cd "$(dirname "$0")/.."

BIN_ARG="${1:-}"
ARCH_ARG="${2:-}"

echo "==> 1/6 检查工具链"
command -v dpkg-deb >/dev/null || {
  echo "未找到 dpkg-deb（apt install dpkg）；本脚本只能在 Debian 系上出 deb" >&2
  exit 1
}

echo "==> 2/6 准备二进制"
if [ -n "$BIN_ARG" ]; then
  # CI 用法：二进制已由矩阵步骤编好，这里只校验存在性，不重复编译、也不查开发包
  BIN="$BIN_ARG"
  [ -f "$BIN" ] || { echo "找不到二进制 $BIN" >&2; exit 1; }
  echo "    使用已编好的二进制：$BIN"
else
  BIN="target/release/rustshell"
  command -v cargo >/dev/null || { echo "未找到 cargo，请先安装 rustup" >&2; exit 1; }

  # 逐个 pkg-config 探测。分两类 —— 缺了都会让 cargo build 失败，但**失败时机差别很大**：
  #   ① build.rs 里 probe 的（x11 / xcb / fontconfig / freetype / dbus-1）：build.rs 用的是
  #      `.unwrap()` 或直接 panic ⇒ 缺了当场就报，提示里常还带着 apt 包名；
  #   ② 只写 `#[link(name = "xkbcommon")]`、压根没有 build.rs 的：**编译期毫无征兆**，
  #      一路编到链接阶段才炸成 `unable to find library -lxkbcommon`（2026-10-16 的 Linux 红）。
  #      这类包同样提供 .pc（xkbcommon.pc / xkbcommon-x11.pc），所以照样能用 pkg-config 探。
  #   ③ cmake：rustls 的默认 provider aws-lc-rs 用 CMake 构建 aws-lc-sys，缺了同样会失败。
  # 在这里提前说清楚，好过让用户在 cargo build 的报错里大海捞针。
  missing=""
  for spec in "dbus-1:libdbus-1-dev" "x11:libx11-dev" "xcb:libxcb1-dev" \
    "xkbcommon:libxkbcommon-dev" "xkbcommon-x11:libxkbcommon-x11-dev" \
    "fontconfig:libfontconfig1-dev" "freetype2:libfreetype-dev"; do
    pc="${spec%%:*}"
    pkg="${spec##*:}"
    if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "$pc"; then
      echo "    ok   $pc"
    else
      echo "    缺失 $pc（apt 包：$pkg）"
      missing="$missing $pkg"
    fi
  done
  if ! command -v cmake >/dev/null 2>&1; then
    echo "    缺失 cmake（apt 包：cmake）"
    missing="$missing cmake"
  fi
  if [ -n "$missing" ]; then
    echo "缺少系统开发包，先执行（Ubuntu / Debian）：" >&2
    echo "  sudo apt-get install -y build-essential pkg-config$missing" >&2
    exit 1
  fi

  cargo build --release -p rustshell
fi

echo "==> 3/6 架构与版本"
# 架构：显式参数优先，否则按本机 uname。产物名里的架构必须与二进制真实架构一致
# ——「名字写 x64、内容是 arm64」在 CI 上是靠 release.yml 的 Verify binary 拦的，
# 这里同理先归一化再对产物本体校验。
if [ -n "$ARCH_ARG" ]; then
  RAW_ARCH="$ARCH_ARG"
else
  RAW_ARCH="$(uname -m)"
fi
case "$RAW_ARCH" in
x64 | amd64 | x86_64) ARCH=x64 ;;
arm64 | aarch64) ARCH=arm64 ;;
*)
  echo "暂不支持的架构：$RAW_ARCH（只出 x64 / arm64 两种产物）" >&2
  exit 1
  ;;
esac
# deb 的 Architecture 字段用的是 Debian 架构名，不是我们文件名里的 x64/arm64。
case "$ARCH" in
x64) DEB_ARCH=amd64 ;;
arm64) DEB_ARCH=arm64 ;;
esac
echo "    架构：$RAW_ARCH -> 文件名 $ARCH / control 里 $DEB_ARCH"

# 版本号提取**不要**用 `sed -E 's/version\s*=\s*"([^"]+)"/\1/'`：`\s` 是 GNU sed
# 扩展，别的 sed（BSD sed）不认 —— 正则匹配不上、替换静默失败，VERSION 会变成
# 整行 `version = "1.0.0"`，产物名里带上双引号（2026-10-16 CI 的 macOS 失败就是这个）。
# awk 按引号切分不涉及任何正则转义，GNU / BSD 行为一致。
VERSION=$(awk -F'"' '/^version/ { print $2; exit }' Cargo.toml)
if [ -z "$VERSION" ]; then
  echo "无法从 Cargo.toml 解析版本号（^version 行缺失或格式变了）" >&2
  exit 1
fi
echo "    版本：$VERSION"

# `file` 在 Ubuntu runner 上一定有；没有就跳过这一步（尽力而为的断言不该让打包失败）
if command -v file >/dev/null 2>&1; then
  DESC=$(file -b "$BIN")
  echo "    $DESC"
  case "$ARCH" in
  x64) echo "$DESC" | grep -q "x86-64" || { echo "产物不是 x86-64，与文件名不符" >&2; exit 1; } ;;
  arm64) echo "$DESC" | grep -q "aarch64" || { echo "产物不是 aarch64，与文件名不符" >&2; exit 1; } ;;
  esac
fi

echo "==> 4/6 组装目录树"
# 打包根放在临时目录：直接往 dist/ 里搭树的话，中途失败会留下半个包，
# 而且下一次 ls dist/ 会看到一堆看似正常的 usr/ DEBIAN/ 目录。
ROOT="$(mktemp -d)"
# 注意两点：
#  · trap 是全局的，而 ROOT 是普通变量 —— 必须在这里把路径**展开**写进 trap，
#    否则退出时清不掉；
#  · 清理失败绝不能让脚本以非零码收场（打包本身已成功，报「失败」会误导），
#    故 `|| true` 兜住。
trap "rm -rf '$ROOT' >/dev/null 2>&1 || true" EXIT

mkdir -p "$ROOT/DEBIAN" \
  "$ROOT/usr/bin" \
  "$ROOT/usr/share/applications" \
  "$ROOT/usr/share/icons/hicolor/256x256/apps" \
  "$ROOT/usr/share/doc/rustshell"

# install 而不是 cp：一步搞定「权限 755 + 覆盖」，省掉一条容易漏的 chmod
install -m 755 "$BIN" "$ROOT/usr/bin/rustshell"
install -m 644 app/assets/rustshell.desktop \
  "$ROOT/usr/share/applications/rustshell.desktop"
install -m 644 app/assets/rustshell.png \
  "$ROOT/usr/share/icons/hicolor/256x256/apps/rustshell.png"
# 版权声明：Debian 政策要求 /usr/share/doc/<pkg>/copyright 存在。
# 直接放仓库根 LICENSE 原文（Apache-2.0），不另写一份 —— 两份文本必然漂移。
install -m 644 LICENSE "$ROOT/usr/share/doc/rustshell/copyright"

# ── control ──────────────────────────────────────────────────────────────────
# Depends 逐个对照 release.yml 里「Install Linux system dependencies」的清单：
#   · 链接期真正需要的（libx11 / libxcb / libxkbcommon(+x11) / fontconfig /
#     freetype / dbus-1）写进 Depends —— 缺了就是 ld.so 直接报
#     `error while loading shared libraries`，装包时就该拦住；
#   · libegl1 / libvulkan1 是**运行时 dlopen**（khronos-egl 的 dynamic、
#     ash 的 loaded），缺了不崩、只是拿不到 GPU 加速，所以只写 Recommends；
#   · 不写 libwayland-client0：wayland-sys 走 dlopen feature，没有硬链接。
#
# libc6 的下限取**构建机**的 glibc 版本：这是「一定能在本机跑」的上界，
# 偏保守（可能比二进制真实需要的更高）。反过来猜一个低值才危险 ——
# 装到旧发行版上会变成 `GLIBC_2.39 not found`。CI 固定 ubuntu-22.04（2.35）。
GLIBC="$( (ldd --version 2>/dev/null || true) | head -n1 | awk '{print $NF}')"
case "$GLIBC" in
[0-9]*.[0-9]*) LIBC_DEP="libc6 (>= $GLIBC)" ;;
*) LIBC_DEP="libc6" ;;
esac

# du -sk 的 KiB 单位正是 control 里 Installed-Size 的约定单位（不是字节）
SIZE_KB="$(du -sk "$ROOT/usr" | cut -f1)"

cat > "$ROOT/DEBIAN/control" <<EOF
Package: rustshell
Version: $VERSION
Architecture: $DEB_ARCH
Maintainer: yuxinyu
Installed-Size: $SIZE_KB
Depends: $LIBC_DEP, libgcc-s1, libx11-6, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libfontconfig1, libfreetype6, libdbus-1-3
Recommends: libegl1, libvulkan1
Section: utils
Priority: optional
Description: RustShell —— SSH / 终端 / SFTP 一体化桌面客户端
 基于 Rust 与 GPUI 的跨平台桌面终端与服务器管理软件。集成 SSH 连接管理、
 多标签终端、SFTP 文件传输与 AI 命令助手。
 .
 本包只提供可执行文件与桌面入口；会话配置保存在用户主目录下。
EOF

echo "==> 5/6 构建 .deb"
OUT="dist/RustShell-linux-$ARCH-$VERSION.deb"
mkdir -p dist
# --root-owner-group：把文件属主统一写成 root:root，否则打出来的包里会带上
# 构建机的 UID/GID，装到别的机器上就是一堆「属于 uid 1001 的文件」。
dpkg-deb --root-owner-group --build "$ROOT" "$OUT"
ls -lh "$OUT"

echo "==> 6/6 校验产物"
# 四个断言：包名结构、二进制路径、桌面入口、图标。任何一条不满足都会让
# 「装上了但启动器里找不到」这种软故障溜过去，所以在这里当场验。
dpkg-deb --info "$OUT" | sed -n '1,12p'
LISTING="$(dpkg-deb --contents "$OUT")"
for want in "./usr/bin/rustshell" \
  "./usr/share/applications/rustshell.desktop" \
  "./usr/share/icons/hicolor/256x256/apps/rustshell.png"; do
  if ! printf '%s\n' "$LISTING" | grep -qF "$want"; then
    echo "包内缺少 $want" >&2
    exit 1
  fi
done
printf '%s\n' "$LISTING" | grep -E 'usr/bin/rustshell$' | grep -q '^-rwxr-xr-x' || {
  echo "usr/bin/rustshell 的可执行位不对（应为 755）" >&2
  exit 1
}
echo "校验通过：$OUT"
echo "完成。安装：sudo apt install ./$OUT    卸载：sudo apt remove rustshell"
