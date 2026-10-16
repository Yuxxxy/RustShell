#!/usr/bin/env python3
"""扫描 Linux 目标依赖闭包里所有「链接期需要系统 .so」的 crate，用来推导构建机要装哪些包。

为什么需要它：Linux 构建的系统依赖有三个来源，**漏掉第二类就会在链接期炸**，
而且报错里不会告诉你要装哪个包（2026-10-16 的 Linux 红即此因）：

  1. build.rs 里调 pkg-config 的（`pkg_config::probe` / `find_library`）
     —— 缺失时 build.rs 当场 panic，提示里通常还带着 apt 包名，属于好查的。
  2. 只写 `#[link(name = "x")]`、**压根没有 build.rs** 的
     —— 编译期毫无征兆，一路编到链接阶段才变成
     `rust-lld: error: unable to find library -lx`。xkbcommon 就是这一类，
     光扫 build.rs 会把它整个漏掉。
  3. 走 CMake / cc 现场构建的 —— 需要 cmake / gcc（如 rustls 默认 provider
     aws-lc-rs 的 aws-lc-sys）。

所以这里三类一起扫：build.rs 的链接指令、build.rs 的 pkg-config 探测、
以及**源码里的 `#[link(name = ...)]` 属性**。

用法：
    python3 scripts/audit-linux-link-libs.py [target-triple]

    # 依赖升级（尤其 gpui-pre 系列）后重推一遍，核对 release.yml 里
    # "Install Linux system dependencies" 的清单是否仍然够用：
    python3 scripts/audit-linux-link-libs.py x86_64-unknown-linux-gnu
    python3 scripts/audit-linux-link-libs.py aarch64-unknown-linux-gnu

注意：本脚本只做**静态推导**，它列出的是「可能需要在构建机装包」的候选；
真正的结论要靠一次 Linux CI run。默认 target 为 x86_64-unknown-linux-gnu。
"""
import json
import re
import subprocess
import sys
from pathlib import Path

TARGET = sys.argv[1] if len(sys.argv) > 1 else "x86_64-unknown-linux-gnu"

meta = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked",
         "--filter-platform", TARGET],
        capture_output=True, text=True, check=True,
    ).stdout
)
pkgs = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}

# 从工作区所有成员出发（rustshell 等）走闭包
roots = [i for i, p in pkgs.items() if p["source"] is None]
seen, stack = set(), list(roots)
while stack:
    pid = stack.pop()
    if pid in seen:
        continue
    seen.add(pid)
    node = nodes.get(pid)
    if not node:
        continue
    for dep in node["deps"]:
        kinds = {k.get("kind") for k in dep.get("dep_kinds", [])}
        # 只关心 normal（运行时 / 链接期）依赖；build 依赖跑在宿主上，不是目标平台的 .so
        if None in kinds or "normal" in kinds:
            stack.append(dep["pkg"])

RE_LINK_LIB = re.compile(
    r'rustc-link-lib\s*(?:=|,)?\s*(?:kind\s*=\s*"\w+"\s*,\s*)?[\s\)]*"?([A-Za-z0-9_.+-]+)"?'
)
RE_PROBE = re.compile(r'(?:probe|find_library)\s*\(\s*"?([A-Za-z0-9_.+\-/><= ]+)"?\s*\)')
RE_LINK_ATTR = re.compile(r'#\[link\(\s*name\s*=\s*"([^"]+)"')

rows = []
missing_src = []
for pid in sorted(seen, key=lambda i: pkgs[i]["name"]):
    p = pkgs[pid]
    root = Path(p["manifest_path"]).parent
    if not root.exists():
        missing_src.append("%s-%s" % (p["name"], p["version"]))
        continue

    libs, probes, attrs = set(), set(), set()
    for buildrs in root.glob("build.rs"):
        txt = buildrs.read_text(encoding="utf-8", errors="replace")
        if "link-lib" in txt or 'println!("cargo:' in txt:
            libs |= {m.group(1) for m in RE_LINK_LIB.finditer(txt)}
        if "pkg" in txt or "probe" in txt:
            probes |= {m.group(1).strip() for m in RE_PROBE.finditer(txt)}
    # 源码里的 #[link(name=...)]：只扫 src/ —— 没有 build.rs 的包只能这样发现
    srcdir = root / "src"
    if srcdir.exists():
        for rs in srcdir.rglob("*.rs"):
            txt = rs.read_text(encoding="utf-8", errors="replace")
            if "#[link(" in txt:
                attrs |= {m.group(1) for m in RE_LINK_ATTR.finditer(txt)}
    if libs or probes or attrs:
        rows.append((p["name"], p["version"], sorted(libs), sorted(probes), sorted(attrs)))

print("目标平台 %s：闭包 %d 个包，其中 %d 个声明了外部链接" % (TARGET, len(seen), len(rows)))
print("源码缺失（未下载）: %d 个%s"
      % (len(missing_src), " -> %s" % missing_src[:10] if missing_src else ""))
print("=" * 100)
for name, ver, libs, probes, attrs in rows:
    print("%s %s" % (name, ver))
    if libs:
        print("    -l (build.rs):       %s" % ", ".join(libs))
    if probes:
        print("    pkg-config/find:     %s" % ", ".join(probes))
    if attrs:
        print("    #[link(name=...)]:   %s" % ", ".join(attrs))
