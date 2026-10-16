#!/usr/bin/env bash
# RustShell Windows 安装包（cargo-packager 0.11）
#
# 用法（从任意目录执行均可，脚本会自动定位仓库根）：
#   ./scripts/package-windows.sh         # NSIS 安装包（.exe，默认）
#   ./scripts/package-windows.sh nsis,wix   # 再出一个 .msi（wix 需本机已装 WiX Toolset）
# 产物：dist/packager/rustshell_<version>_x64-setup.exe
#   （文件名是「二进制名_版本_架构-setup.exe」，由 packager 固定拼出）
#
# 前置（只需一次）：
#   cargo install cargo-packager --locked
#   NSIS 工具链**不需要**手工安装：packager 会自己拉，本脚本另在打包前补一次缺
#   （见下方「2/4 NSIS 工具链预置」——国内直连 github.com 会超时，必须兜住）。
#
# 从哪个目录执行都行：脚本先 cd 到**仓库根**再跑 packager。
#   packager 自己会把工作目录切到 app/（crate 目录），因此配置里的
#   `icons` / `installer-icon` 相对 app/ 解析（写 `app/assets/...` 会报
#   「系统找不到指定的路径」），而 `out-dir = "../dist/packager"` 相对 app/
#   解析后正好落在**仓库根的 dist/packager/**。
#   配置在 app/Cargo.toml 的 `[package.metadata.packager]`，那里有更详细的说明。
#
# 配置入口：app/Cargo.toml 的 `[package.metadata.packager]`
#   —— 产品名 / 标识符 / 图标 / 输出目录 / 格式 / 打包前构建命令都在那里，
#   本脚本只负责「调用 + 校验产物 + 报告路径」，不做二次拼接（口径只有一处）。

set -euo pipefail
# 脚本位于 <repo>/scripts/，必须回到仓库根：VERSION 从根 Cargo.toml 读，
# packager 也需要仓库根作为工作目录。
cd "$(dirname "$0")/.."

FORMATS="${1:-nsis}"

echo "==> 1/4 检查工具链"
command -v cargo >/dev/null || { echo "未找到 cargo，请先安装 rustup"; exit 1; }
if ! cargo packager --version >/dev/null 2>&1; then
  echo "未安装 cargo-packager。先执行："
  echo "    cargo install cargo-packager --locked"
  exit 1
fi
cargo packager --version

# 版本号提取**不要**用 `sed -E 's/version\s*=\s*"([^"]+)"/\1/'`：`\s` 是 GNU sed
# 扩展，BSD sed（macOS 自带 /usr/bin/sed）不认它 —— 会把 `\s` 当字面量 `s`，
# 正则匹配不上、替换静默失败，`VERSION` 会变成整行 `version = "1.0.0"`，
# 产物名里带上双引号（2026-10-16 CI 的 macOS 失败就是这个）。awk 按引号切分
# 不涉及任何正则转义，GNU / BSD 行为一致。
VERSION=$(awk -F'"' '/^version/ { print $2; exit }' Cargo.toml)
if [ -z "$VERSION" ]; then
  echo "无法从根 Cargo.toml 解析版本号（^version 行缺失或格式变了）" >&2
  exit 1
fi
echo "    版本：$VERSION"
echo "    格式：$FORMATS"

# ══════════════════════════════════════════════════════════════════════════════
# 2/4  NSIS 工具链预置
# ══════════════════════════════════════════════════════════════════════════════
# 背景（2026-10-13 实测）：cargo-packager 不内置 NSIS，首次打包时它从 GitHub
# Releases 拉三个东西解到 $LOCALAPPDATA/.cargo-packager/NSIS/：
#     nsis-3.09.zip            → makensis.exe / Stubs / Include
#     NSIS-ApplicationID.zip   → Plugins/x86-unicode/ApplicationID.dll
#     nsis_tauri_utils.dll     → Plugins/x86-unicode/nsis_tauri_utils.dll
# 三个 URL 全在 github.com 上，而 packager **没有任何镜像/代理开关**。国内直连
# 会超时（os error 10060），整包中止，只留一句 "Network Error"。
#
# 更坑的是：只要 11 个必需文件里有任何一个缺失，packager 会先 `remove_dir_all`
# 整个 NSIS 目录再重下（源码 src/package/nsis/mod.rs 的 package()）。所以一次
# 插件下载失败，会把上一份本来能用的工具链一起删掉 —— 越重试越坏。
#
# 对策：打包前本脚本自己补齐。补齐后 packager 命中「文件齐全 + 哈希一致」分支，
# **完全不联网**。下载走国内可达的 GitHub 代理，逐个试（GH_MIRRORS）。
# 失败时不致命 —— 仍把机会留给 packager 自己拉，只是会多等一次超时。
NSIS_DIR="${LOCALAPPDATA:-$HOME/AppData/Local}/.cargo-packager/NSIS"
NSIS_PLUGINS="$NSIS_DIR/Plugins/x86-unicode"

# packager 的必需文件清单（与其源码 NSIS_REQUIRED_FILES 保持一致）；
# 前 9 项来自 nsis-3.09.zip，后 2 项是插件，本脚本分别处理。
NSIS_BASE_FILES=(
  "makensis.exe"
  "Bin/makensis.exe"
  "Stubs/lzma-x86-unicode"
  "Stubs/lzma_solid-x86-unicode"
  "Include/MUI2.nsh"
  "Include/FileFunc.nsh"
  "Include/x64.nsh"
  "Include/nsDialogs.nsh"
  "Include/WinMessages.nsh"
)
# 上游为 nsis_tauri_utils.dll 与 nsis-3.09.zip 公布了 SHA1，本脚本照抄并校验；
# ApplicationID.dll 上游没给哈希，只按「文件存在且非空」判断。
UTILS_SHA1="53a7cfaeb6a4a9653d6d5fbff02a3c3b8720130a"
NSIS_ZIP_SHA1="586855a743a6e0ade203d8758af303a48ee0716b"

# GitHub 代理前缀，按可达性从优到劣。保留空前缀（直连）—— 在能直连 github.com
# 的环境里它就是首选，代理反而是多余的绕路。换/加代理只改这一行。
GH_MIRRORS=("" "https://gh-proxy.com/" "https://ghproxy.net/")

# fetch_github <github 路径> <输出文件>：依次尝试直连与各镜像，任一成功即返回
# curl 自身的报错（超时/断流）压掉 —— 下面每个镜像都会回一行「× 不可达」，够用了
fetch_github() {
  local path="$1" out="$2" prefix url
  for prefix in "${GH_MIRRORS[@]}"; do
    url="${prefix}https://github.com/$path"
    if curl -fsSL --retry 2 --connect-timeout 10 --max-time 300 -o "$out" "$url" 2>/dev/null; then
      return 0
    fi
    echo "      × 不可达：${prefix:-（直连 github.com）}"
  done
  return 1
}

# verify_sha1 <文件> <期望值>：一致返回 0
# ⚠️ GNU coreutils 的 sha1sum 在**文件名含反斜杠**时会以 `\` 开头输出整行、
#    并把文件名里的 `\` 转义成 `\\`（Windows 路径必然触发）。直接 cut 会得到
#    `\53a7cf...` 这种带前缀的假哈希 —— 必须先剥掉首字符再比。
verify_sha1() {
  local got
  got="$(sha1sum "$1" | cut -d' ' -f1 | tr -d '\\' | tr 'A-Z' 'a-z')"
  if [ "$got" = "$2" ]; then
    return 0
  fi
  echo "      SHA1 校验失败：期望 $2，实际 $got（镜像可能返回了错误内容）"
  return 1
}

provision_nsis() {
  local need_base=0 need_aid=0 need_utils=0

  for f in "${NSIS_BASE_FILES[@]}"; do
    [ -e "$NSIS_DIR/$f" ] || need_base=1
  done
  [ -s "$NSIS_PLUGINS/ApplicationID.dll" ] || need_aid=1
  # 插件的哈希由 packager 自己校验，这里先对一遍，免得补齐了却仍被判定「不一致」
  # 而触发它删目录重下（那样就白补了）。
  if ! [ -s "$NSIS_PLUGINS/nsis_tauri_utils.dll" ] \
     || ! verify_sha1 "$NSIS_PLUGINS/nsis_tauri_utils.dll" "$UTILS_SHA1" >/dev/null 2>&1; then
    need_utils=1
  fi

  if [ "$need_base" -eq 0 ] && [ "$need_aid" -eq 0 ] && [ "$need_utils" -eq 0 ]; then
    echo "==> 2/4 NSIS 工具链就绪，跳过下载（$NSIS_DIR）"
    return 0
  fi

  echo "==> 2/4 补齐 NSIS 工具链"
  command -v curl >/dev/null    || { echo "    需要 curl，跳过补齐"; return 0; }
  command -v sha1sum >/dev/null || { echo "    需要 sha1sum，跳过补齐"; return 0; }

  local TMP
  TMP="$(mktemp -d)"
  # 注意两点：
  #  · trap 是全局的，而 TMP 是 local —— 必须在这里把路径**展开**写进 trap，
  #    否则函数返回后变量已消失，退出时清不掉临时目录；
  #  · 清理失败绝不能让脚本以非零码收场（打包本身已成功，报「失败」会误导），
  #    故 `|| true` 兜住。
  trap "rm -rf '$TMP' >/dev/null 2>&1 || true" EXIT

  # ── 2a. 基础工具链：用官方 zip 覆盖式补齐（zip 里的根目录就叫 nsis-3.09） ────
  if [ "$need_base" -eq 1 ]; then
    echo "    · nsis-3.09.zip（makensis / Stubs / Include）"
    if fetch_github "tauri-apps/binary-releases/releases/download/nsis-3.9/nsis-3.09.zip" "$TMP/nsis.zip" \
       && verify_sha1 "$TMP/nsis.zip" "$NSIS_ZIP_SHA1"; then
      # 解到独立子目录：$TMP 是新建的，不必先删；也更不容易踩到「删到一半」的状态
      unzip -q "$TMP/nsis.zip" -d "$TMP/x"
      local src="$TMP/x/nsis-3.09"
      [ -d "$src" ] || src="$(find "$TMP/x" -maxdepth 1 -mindepth 1 -type d | head -n1)"
      if [ -d "$src" ]; then
        # 刻意**不**先删 $NSIS_DIR：官方 zip 是完整树，直接覆盖即可补齐全部必需文件；
        # 少一步删除，就少一次「删完却没下下来」的风险（packager 正是栽在这里）。
        mkdir -p "$NSIS_DIR"
        cp -rf "$src/." "$NSIS_DIR/"
        echo "      已补齐 → $NSIS_DIR"
        need_aid=1; need_utils=1   # 插件不在 zip 里，必然要单独装
      else
        echo "      解压后未找到 nsis-3.09 目录，改为交给 cargo-packager 自行下载"
      fi
    else
      echo "      下载/校验失败，改为交给 cargo-packager 自行下载"
    fi
  fi

  # ── 2b. 插件：按需单独补 ───────────────────────────────────────────────────
  mkdir -p "$NSIS_PLUGINS"

  if [ "$need_aid" -eq 1 ] && ! [ -s "$NSIS_PLUGINS/ApplicationID.dll" ]; then
    echo "    · ApplicationID.dll"
    if fetch_github "tauri-apps/binary-releases/releases/download/nsis-plugins-v0/NSIS-ApplicationID.zip" "$TMP/aid.zip"; then
      # 解到独立子目录，不 junk 路径、也不需要先删旧文件
      unzip -qo "$TMP/aid.zip" "ReleaseUnicode/ApplicationID.dll" -d "$TMP/aid"
      if [ -s "$TMP/aid/ReleaseUnicode/ApplicationID.dll" ]; then
        cp -f "$TMP/aid/ReleaseUnicode/ApplicationID.dll" "$NSIS_PLUGINS/ApplicationID.dll"
        echo "      已安装 → Plugins/x86-unicode/ApplicationID.dll"
      else
        echo "      压缩包里没有 ReleaseUnicode/ApplicationID.dll，跳过"
      fi
    else
      echo "      下载失败，跳过（packager 会自行重试）"
    fi
  fi

  if [ "$need_utils" -eq 1 ] && ! [ -s "$NSIS_PLUGINS/nsis_tauri_utils.dll" ]; then
    echo "    · nsis_tauri_utils.dll"
    if fetch_github "tauri-apps/nsis-tauri-utils/releases/download/nsis_tauri_utils-v0.2.1/nsis_tauri_utils.dll" "$TMP/utils.dll" \
       && verify_sha1 "$TMP/utils.dll" "$UTILS_SHA1"; then
      cp -f "$TMP/utils.dll" "$NSIS_PLUGINS/nsis_tauri_utils.dll"
      echo "      已安装 → Plugins/x86-unicode/nsis_tauri_utils.dll"
    else
      echo "      下载/校验失败，跳过（packager 会自行重试）"
    fi
  fi

  # 补齐失败时必须把后果说清楚：packager 一旦发现必需文件缺失，会先删掉整个 NSIS
  # 目录再重下 —— 也就是说它接下来大概率会再失败一次，而且顺手毁掉现有工具链。
  if ! [ -s "$NSIS_PLUGINS/ApplicationID.dll" ] || ! [ -s "$NSIS_PLUGINS/nsis_tauri_utils.dll" ]; then
    echo "    ⚠ 插件仍不完整：cargo-packager 接下来会删除 $NSIS_DIR 并从 github.com 重下，"
    echo "      大概率再次超时。可先设置代理后重跑本脚本："
    echo "        export https_proxy=http://127.0.0.1:<端口>"
  fi
}

provision_nsis

echo "==> 3/4 打包（构建 release 由配置里的 before-packaging-command 负责）"
cargo packager --release --formats "$FORMATS"

echo "==> 4/4 校验产物"
# 产物落在仓库根的 dist/packager/（app/Cargo.toml 里 out-dir = "../dist/packager"，
# 相对 packager 的工作目录 app/ 解析）
OUT_DIR="dist/packager"
# 若 packager 改了产物命名规则，这里会明确报出来，而不是让人以为「打包没跑」
if ! ls "$OUT_DIR"/* >/dev/null 2>&1; then
  echo "打包结束但 $OUT_DIR 里没有产物 —— 检查上面的 packager 输出"
  exit 1
fi
ls -lh "$OUT_DIR"/

cat <<EOF

完成。安装包在 $OUT_DIR/ 下：
  · NSIS 安装包（.exe）：装到当前用户目录，不弹 UAC
  · 安装前请先按需卸载旧版本；本安装包未做代码签名，SmartScreen 会提示「未知发布者」
EOF
