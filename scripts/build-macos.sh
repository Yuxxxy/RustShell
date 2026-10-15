#!/usr/bin/env bash
# RustShell macOS 本地构建脚本（在 Mac 上执行）
#
# 为什么需要真机/CI：gpui-pre 的 macOS 后端（gpui_macos）依赖 Cocoa/AppKit
# 系统框架，只能在本机或 macOS runner 上链接。Windows 无法交叉编译 macOS 应用。
#
# 用法（从任意目录执行均可，脚本会自动定位仓库根）：
#   ./scripts/build-macos.sh
# 产物：dist/rustshell-<version>-macos-x64-arm64.zip（通用二进制，Intel/Apple Silicon 均可运行）
#
# 前置要求：已安装 rustup（stable toolchain）+ Xcode Command Line Tools
#   xcode-select --install
#   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

set -euo pipefail
# 脚本位于 <repo>/scripts/，必须回到仓库根：VERSION 从根 Cargo.toml 读，
# cargo 与产物目录 dist/ 也都相对仓库根。
cd "$(dirname "$0")/.."

echo "==> 1/4 检查工具链"
command -v cargo >/dev/null || { echo "未找到 cargo，请先安装 rustup"; exit 1; }
if ! rustup target list --installed | grep -q "x86_64-apple-darwin"; then
  echo "   添加 x86_64-apple-darwin 目标（编译通用二进制需要）"
  rustup target add x86_64-apple-darwin
fi
rustup target add aarch64-apple-darwin 2>/dev/null || true

VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/version\s*=\s*"([^"]+)"/\1/')
echo "==> 2/4 构建 release（版本 $VERSION）"
cargo build --release -p rustshell
cargo build --release -p rustshell --target x86_64-apple-darwin

echo "==> 3/4 合并通用二进制（universal binary）"
mkdir -p target/universal-apple-darwin/release
lipo -create \
  "target/aarch64-apple-darwin/release/rustshell" \
  "target/x86_64-apple-darwin/release/rustshell" \
  -output "target/universal-apple-darwin/release/rustshell"
file "target/universal-apple-darwin/release/rustshell"

echo "==> 4/4 打包"
STAGE="dist/rustshell-$VERSION-macos-x64-arm64"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "target/universal-apple-darwin/release/rustshell" "$STAGE/"
# 去掉 macOS 的 quarantine 属性，避免“无法打开，因为无法验证开发者”
xattr -dr com.apple.quarantine "$STAGE" 2>/dev/null || true
cd dist
rm -f "rustshell-$VERSION-macos-x64-arm64.zip"
/usr/bin/zip -r -y "rustshell-$VERSION-macos-x64-arm64.zip" "rustshell-$VERSION-macos-x64-arm64"
cd ..
ls -lh "dist/rustshell-$VERSION-macos-x64-arm64.zip"
echo "完成：dist/rustshell-$VERSION-macos-x64-arm64.zip（通用二进制，解压后在终端运行 ./rustshell）"
