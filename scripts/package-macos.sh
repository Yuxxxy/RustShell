#!/usr/bin/env bash
# RustShell macOS 安装包（通用二进制 → .app → .dmg）
#
# 用法（只能在 macOS 上执行，从任意目录均可 —— 脚本会自动定位仓库根）：
#   ./scripts/package-macos.sh                    # 编译通用二进制并打包（本地用法）
#   ./scripts/package-macos.sh <二进制路径>        # 只打包已编好的二进制（CI 用法）
#
# 产物：dist/RustShell-<version>-macos-universal.dmg
#   命名规则全仓唯一：`RustShell-<版本号>-<平台>-<架构>.<扩展名>`
#   （macOS 出的是通用二进制，架构段写 universal）。
#
# 为什么是 .dmg 而不是 .zip：
#   用户下载一个 .zip 解开后拿到的是一个裸二进制，双击只会弹终端、也不会进
#   「应用程序」；.dmg 里是标准 RustShell.app 包 + 一个 /Applications 软链，
#   拖进去就能用 —— 这才是 macOS 用户预期的「安装文件」。
#
# 前置：hdiutil / codesign / ditto / lipo（macOS 自带，Xcode Command Line Tools 会装齐）

set -euo pipefail
cd "$(dirname "$0")/.."

BIN_ARG="${1:-}"

# 反域名标识必须与 app/Cargo.toml 的 `[package.metadata.packager] identifier` 一致：
# 它是这台机器上「是不是同一个 App」的判据（LaunchServices / 通知 / 单实例都认它），
# 与 Windows 安装包用两个不同标识会让后续统一身份变得很麻烦。
BUNDLE_ID="com.rustshell.desktop"
APP_NAME="RustShell"

echo "==> 1/6 检查工具链"
for tool in hdiutil codesign ditto lipo; do
  command -v "$tool" >/dev/null || {
    echo "未找到 ${tool}（macOS 自带；缺失通常意味着 Xcode Command Line Tools 没装）" >&2
    exit 1
  }
done

# 版本号提取**不要**用 `sed -E 's/version\s*=\s*"([^"]+)"/\1/'`：`\s` 是 GNU sed
# 扩展，BSD sed（macOS 自带 /usr/bin/sed）不认它 —— 会把 `\s` 当字面量 `s`，
# 正则匹配不上、替换静默失败，VERSION 会变成整行 `version = "1.0.0"`，
# 产物名里带上双引号（2026-10-16 CI 的 macOS 失败就是这个）。
# awk 按引号切分不涉及任何正则转义，GNU / BSD 行为一致。
VERSION=$(awk -F'"' '/^version/ { print $2; exit }' Cargo.toml)
if [ -z "$VERSION" ]; then
  echo "无法从 Cargo.toml 解析版本号（^version 行缺失或格式变了）" >&2
  exit 1
fi
# CFBundleVersion 只接受「点分数字」（1.0.0 可以，1.0.0-beta.1 不行），
# 而 CFBundleShortVersionString 允许预发布后缀。所以两份字段用两个值：
# 版本号里一旦出现 `-` / `+`，CFBundleVersion 就退化成数字前缀。
CF_VERSION="${VERSION%%[+-]*}"
# ⚠️ `${VAR}` 的花括号不是风格问题，是**正确性**问题：macOS runner 上的 bash 是
# 3.2（/bin/bash），它把紧跟 `$VAR` 的全角括号当成变量名的一部分 ——
# `"$VERSION（CFBundle...）"` 会被解析成变量 `VERSION（`，`set -u` 直接报
# `VERSION?: unbound variable`（2026-10-18 首次跑 macOS job 就栽在这行）。
# Linux 的 bash 5 + glibc 不会这样，所以同一个脚本只有 macOS 红。
# 规则：**`$VAR` 后面只要是非 ASCII 字符，一律写成 `${VAR}`**。
echo "    版本：${VERSION}（CFBundleVersion 用 ${CF_VERSION}）"

echo "==> 2/6 准备通用二进制"
if [ -n "$BIN_ARG" ]; then
  # CI 用法：二进制已由矩阵步骤编好，这里只校验，不重复编译
  BIN="$BIN_ARG"
  [ -f "$BIN" ] || { echo "找不到二进制 $BIN" >&2; exit 1; }
  echo "    使用已编好的二进制：$BIN"
else
  BIN="target/universal-apple-darwin/release/rustshell"
  command -v cargo >/dev/null || { echo "未找到 cargo，请先安装 rustup" >&2; exit 1; }

  # 两条架构都要**显式** --target 编一遍。
  # 别省成「先 cargo build 编 host、再把 target/release 当成其中一条架构」——
  # host 构建的产物落在 target/release/，不在 target/<triple>/release/，
  # 于是 lipo 会去读一个不存在的文件（Intel / Apple Silicon 是反过来的同一种错）。
  for triple in aarch64-apple-darwin x86_64-apple-darwin; do
    if ! rustup target list --installed | grep -qx "$triple"; then
      echo "    添加目标 $triple"
      rustup target add "$triple"
    fi
  done
  cargo build --release -p rustshell --target aarch64-apple-darwin
  cargo build --release -p rustshell --target x86_64-apple-darwin
  mkdir -p target/universal-apple-darwin/release
  lipo -create \
    target/aarch64-apple-darwin/release/rustshell \
    target/x86_64-apple-darwin/release/rustshell \
    -output "$BIN"
fi

# 产物名里写了 universal，就必须两条架构都在 —— 否则 Intel 机器上双击直接
# `bad CPU type in executable`。CI 侧同一断言在 release.yml 的 Verify binary。
ARCHS="$(lipo -archs "$BIN")"
echo "    包含架构：$ARCHS"
for want in x86_64 arm64; do
  case "$ARCHS" in
  *"$want"*) ;;
  *)
    echo "产物缺少 $want 架构（名字里的 macos-universal 名不副实）" >&2
    exit 1
    ;;
  esac
done

echo "==> 3/6 组装 $APP_NAME.app"
# 打包根放临时目录：中途失败不会在 dist/ 里留下半个 .app（那东西双击是能启动的，
# 看起来像正常产物，最容易糊弄过去）。
ROOT="$(mktemp -d)"
# trap 是全局的，路径必须在这里**展开**写进去；清理失败不能影响退出码。
trap "rm -rf '$ROOT' >/dev/null 2>&1 || true" EXIT

APP="$ROOT/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$BIN" "$APP/Contents/MacOS/$APP_NAME"
[ -f app/assets/RustShell.icns ] || {
  echo "缺 app/assets/RustShell.icns（生成：python3 scripts/gen-app-icons.py）" >&2
  exit 1
}
install -m 644 app/assets/RustShell.icns "$APP/Contents/Resources/RustShell.icns"
# 老式四字符类型/创建者码。现代 macOS 不读它，但某些第三方工具（以及
# `file` 命令对 .app 的识别）会看；内容固定是 APPL + 四个问号。
printf 'APPL????' > "$APP/Contents/PkgInfo"

# ── Info.plist ───────────────────────────────────────────────────────────────
# 刻意**不写** LSMinimumSystemVersion：我们没有实测过 gpui-pre-macos 的最低
# 系统版本，随便填一个（11.0 / 12.0）就是一句无法自证的假话。不写等于不设限，
# 系统版本不够时由 dyld 报真实的符号缺失，比一份假的声明更容易排查。
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleExecutable</key>
    <string>$APP_NAME</string>
    <key>CFBundleIconFile</key>
    <string>RustShell</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$CF_VERSION</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>NSHumanReadableCopyright</key>
    <string>Copyright 2026 yuxinyu</string>
</dict>
</plist>
EOF
# plutil 是 macOS 自带的 plist 校验器：输出 OK 才说明这份 XML 能被系统解析。
# 一个手写的 plist 里少个闭合标签，症状是「双击没反应」，且不留任何日志。
plutil -lint "$APP/Contents/Info.plist"

echo "==> 4/6 临时签名（ad-hoc）"
# Apple Silicon 上 arm64 可执行文件**必须**带有效签名才会被内核放行。rustc 链接时
# 会给 arm64 产物附一个 ad-hoc 签名，但 .app 是后组装的，用 codesign 重新走一遍
# 才能把 Info.plist 一起纳入签名范围。
# 这是 ad-hoc（--sign -），**不是**开发者签名：不含 Team ID、不能过 Gatekeeper 的
# 首次运行检查，见文件末尾的提示。
xattr -cr "$APP" 2>/dev/null || true
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP"
codesign --verify --strict "$APP"
# codesign -dv 把信息打到 stderr；grep 命不中时管道会返回非零 ——
# 在 set -o pipefail 下那就等于整步失败，所以末尾 `|| true` 兜住（这只是打印）
echo "    $(codesign -dv "$APP" 2>&1 | grep -E '^(Identifier|Signature)' | tr '\n' ' ' || true)"

echo "==> 5/6 构建 .dmg"
DMGROOT="$ROOT/dmg"
mkdir -p "$DMGROOT"
# ditto 而不是 cp -R：它会保留扩展属性与资源分支，是 Apple 推荐的拷 .app 的方式
# （cp -R 在部分 macOS 版本上会打散 bundle 的 xattr，导致签名失效）。
ditto "$APP" "$DMGROOT/$APP_NAME.app"
# /Applications 软链：这是 dmg 的「拖拽安装」惯例，用户把图标拖过去就装好了
ln -s /Applications "$DMGROOT/Applications"

OUT="dist/RustShell-$VERSION-macos-universal.dmg"
mkdir -p dist
rm -f "$OUT"
# -fs HFS+：显式指定文件系统，不依赖 hdiutil 在不同 macOS 版本上的默认值
# -format UDZO：压缩的只读镜像（分发的标准格式）
hdiutil create -volname "$APP_NAME" -srcfolder "$DMGROOT" -fs HFS+ -format UDZO -ov -quiet "$OUT"
# verify 会逐块校验 CRC —— 镜像损坏的典型症状是「下载完挂载时报 no mountable
# file systems」，在这里拦下来比让用户去猜要好
hdiutil verify "$OUT" >/dev/null
ls -lh "$OUT"

echo "==> 6/6 校验产物"
# 把 dmg 挂起来，直接看里面到底有什么（而不是相信 create 的返回码）
MNT="$(mktemp -d)"
trap "rm -rf '$ROOT' '$MNT' >/dev/null 2>&1 || true; hdiutil detach '$MNT' >/dev/null 2>&1 || true" EXIT
hdiutil attach "$OUT" -nobrowse -readonly -mountpoint "$MNT" -quiet
ls -1 "$MNT"
[ -d "$MNT/$APP_NAME.app" ] || { hdiutil detach "$MNT" -quiet || true; echo "dmg 里没有 $APP_NAME.app" >&2; exit 1; }
[ -L "$MNT/Applications" ] || { hdiutil detach "$MNT" -quiet || true; echo "dmg 里缺 /Applications 软链" >&2; exit 1; }
hdiutil detach "$MNT" -quiet

cat <<EOF
校验通过：$OUT

完成。分发注意事项（未做开发者签名与公证）：
  · 用户首次打开需「右键 → 打开」，或在终端执行
      xattr -dr com.apple.quarantine /Applications/RustShell.app
  · 要彻底免掉这一步，需要 Apple Developer Program 的 Developer ID 证书，
    并在 CI 里做 codesign --deep --options runtime + notarytool 公证。
EOF
